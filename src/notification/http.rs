//! Outgoing request description shared by the HTTP providers, plus small
//! config-reading helpers. Providers build [`OutRequest`]s with pure
//! functions (unit-testable without a network); [`execute`] sends them.

use std::time::Duration;

use reqwest::Method;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Per-request timeout for every provider call.
pub const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq)]
pub struct OutRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: OutBody,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutBody {
    Empty,
    Json(Value),
    Form(Vec<(String, String)>),
    Raw { content_type: String, body: String },
}

impl OutRequest {
    pub fn post_json(url: impl Into<String>, body: Value) -> Self {
        OutRequest {
            method: Method::POST,
            url: url.into(),
            headers: Vec::new(),
            body: OutBody::Json(body),
        }
    }

    pub fn post_form(url: impl Into<String>, fields: Vec<(String, String)>) -> Self {
        OutRequest {
            method: Method::POST,
            url: url.into(),
            headers: Vec::new(),
            body: OutBody::Form(fields),
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn bearer(self, token: &str) -> Self {
        self.header("Authorization", format!("Bearer {token}"))
    }

    pub fn basic_auth(self, user: &str, pass: &str) -> Self {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        self.header("Authorization", format!("Basic {encoded}"))
    }

    /// The JSON body, if any (handy in tests).
    pub fn json(&self) -> Option<&Value> {
        match &self.body {
            OutBody::Json(v) => Some(v),
            _ => None,
        }
    }

    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Sends one request; non-2xx is an error carrying a short, scrubbed snippet
/// of the response body. Error strings never contain the URL or any of
/// `secrets` (they end up in logs).
pub async fn execute(
    http: &reqwest::Client,
    req: &OutRequest,
    secrets: &[String],
) -> Result<(), String> {
    let mut builder = http.request(req.method.clone(), &req.url).timeout(TIMEOUT);
    for (name, value) in &req.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder = match &req.body {
        OutBody::Empty => builder,
        OutBody::Json(v) => builder.json(v),
        OutBody::Form(fields) => builder.form(fields),
        OutBody::Raw { content_type, body } => builder
            .header(reqwest::header::CONTENT_TYPE, content_type.as_str())
            .body(body.clone()),
    };
    let resp = builder
        .send()
        .await
        .map_err(|e| scrub(&describe_reqwest(e), secrets))?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = read_capped(resp, ERROR_BODY_MAX).await;
    let snippet = snippet(&String::from_utf8_lossy(&body));
    let msg = if snippet.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {snippet}")
    };
    Err(scrub(&msg, secrets))
}

/// Error bodies are only read for a short snippet; a hostile or broken
/// endpoint can't make us buffer more than this.
const ERROR_BODY_MAX: usize = 8 * 1024;

/// Reads at most `cap` bytes of the body (read errors end it early).
async fn read_capped(mut resp: reqwest::Response, cap: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        let room = cap.saturating_sub(buf.len());
        if chunk.len() >= room {
            buf.extend_from_slice(&chunk[..room]);
            break;
        }
        buf.extend_from_slice(&chunk);
    }
    buf
}

pub async fn execute_all(
    http: &reqwest::Client,
    reqs: &[OutRequest],
    secrets: &[String],
) -> Result<(), String> {
    for req in reqs {
        execute(http, req, secrets).await?;
    }
    Ok(())
}

fn describe_reqwest(err: reqwest::Error) -> String {
    if err.is_timeout() {
        return format!("timed out after {}s", TIMEOUT.as_secs());
    }
    let err = err.without_url();
    if err.is_connect() {
        let mut innermost = std::error::Error::source(&err);
        while let Some(next) = innermost.and_then(std::error::Error::source) {
            innermost = Some(next);
        }
        return match innermost {
            Some(cause) => format!("couldn’t connect ({cause})"),
            None => "couldn’t connect".to_string(),
        };
    }
    let mut msg = err.to_string();
    let mut source = std::error::Error::source(&err);
    while let Some(s) = source {
        let part = s.to_string();
        if !msg.contains(&part) {
            msg.push_str(": ");
            msg.push_str(&part);
        }
        source = s.source();
    }
    msg
}

/// First ~200 characters of a response body on one line.
fn snippet(body: &str) -> String {
    let flat: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(200) {
        Some((i, _)) => format!("{}…", &flat[..i]),
        None => flat,
    }
}

/// Replaces every secret (and its URL-encoded form) with a mask.
pub fn scrub(msg: &str, secrets: &[String]) -> String {
    let mut out = msg.to_string();
    for secret in secrets.iter().filter(|s| s.len() >= 4) {
        out = out.replace(secret.as_str(), "•••");
        let encoded = urlencode(secret);
        if encoded != *secret {
            out = out.replace(&encoded, "•••");
        }
    }
    out
}

pub fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Percent-encodes one URL path segment.
pub fn path_segment(s: &str) -> String {
    const KEEP: &[u8] = b"-._~";
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || KEEP.contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// `base` without trailing slashes + `path` (which starts with `/`).
pub fn join(base: &str, path: &str) -> String {
    format!("{}{path}", base.trim().trim_end_matches('/'))
}

/// HMAC-SHA256 (RFC 2104) on top of `sha2`.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

// -- config helpers ------------------------------------------------------------

/// A non-empty (trimmed) string value.
pub fn opt<'a>(config: &'a Value, key: &str) -> Option<&'a str> {
    config
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// A required string value.
pub fn req<'a>(config: &'a Value, key: &str) -> Result<&'a str, String> {
    opt(config, key).ok_or_else(|| format!("missing '{key}' in channel config"))
}

/// An integer stored as a number or a numeric string.
pub fn num(config: &Value, key: &str) -> Option<i64> {
    match config.get(key)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

pub fn flag(config: &Value, key: &str) -> bool {
    match config.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true" || s == "on" || s == "1",
        _ => false,
    }
}

/// Comma/newline separated list.
pub fn list(config: &Value, key: &str) -> Vec<String> {
    opt(config, key)
        .map(|s| {
            s.split([',', '\n', ';'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// A string → string object (e.g. custom headers).
pub fn pairs(config: &Value, key: &str) -> Vec<(String, String)> {
    config
        .get(key)
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        v.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| v.to_string()),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn error_bodies_are_read_only_up_to_the_cap() {
        use axum::body::Body;
        use futures_util::stream;
        // An endless 500 body: must not be buffered (or waited for) in full.
        let app = axum::Router::new().fallback(|| async {
            let chunks = stream::repeat_with(|| Ok::<_, std::io::Error>(vec![b'x'; 4096]));
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Body::from_stream(chunks),
            )
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let http = reqwest::Client::new();
        let resp = http.get(format!("http://{addr}/")).send().await.unwrap();
        let body = tokio::time::timeout(Duration::from_secs(5), read_capped(resp, ERROR_BODY_MAX))
            .await
            .unwrap();
        assert_eq!(body.len(), ERROR_BODY_MAX);
        let req = OutRequest::post_json(format!("http://{addr}/"), serde_json::json!({}));
        let err = tokio::time::timeout(Duration::from_secs(5), execute(&http, &req, &[]))
            .await
            .unwrap()
            .unwrap_err();
        assert!(err.starts_with("HTTP 500") && err.len() < 400, "{err}");
    }

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex::encode(mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn scrub_hides_plain_and_encoded_secrets() {
        let secrets = vec!["s3cr3t/key".to_string()];
        assert_eq!(
            scrub("bad token s3cr3t/key and s3cr3t%2Fkey", &secrets),
            "bad token ••• and •••"
        );
    }

    #[test]
    fn snippet_is_single_line_and_bounded() {
        assert_eq!(snippet("  a\n b  "), "a b");
        assert_eq!(snippet(&"x".repeat(300)).chars().count(), 201);
    }

    #[test]
    fn path_segment_encodes_matrix_room_ids() {
        assert_eq!(path_segment("!abc:matrix.org"), "%21abc%3Amatrix.org");
        assert_eq!(join("https://hs.example/", "/x"), "https://hs.example/x");
    }

    #[test]
    fn config_helpers_read_loose_values() {
        let c = serde_json::json!({"a": " x ", "n": "42", "m": 7, "b": "true", "l": "a, b;c\n", "e": ""});
        assert_eq!(opt(&c, "a"), Some("x"));
        assert_eq!(opt(&c, "e"), None);
        assert_eq!(num(&c, "n"), Some(42));
        assert_eq!(num(&c, "m"), Some(7));
        assert!(flag(&c, "b"));
        assert_eq!(list(&c, "l"), ["a", "b", "c"]);
        assert!(req(&c, "missing").is_err());
    }
}
