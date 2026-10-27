//! HTTP(S) family checker: `http`, `keyword`, `json_query`.
//!
//! Supports method, JSON-object headers, body (json/form/raw encoding),
//! basic auth, OAuth2 client credentials and mTLS (see [`super::http_auth`]),
//! proxy (http/https/socks5/socks5h URL), redirect limit
//! (0 = don't follow; the 3xx response itself is then checked against the
//! accepted codes), "ignore TLS errors", accepted status codes
//! (`200-299,301,418`, `2xx`). The TLS leaf certificate is reported as
//! [`ExpiryInfo`] on every HTTPS check. Environment proxy variables are NOT
//! honoured; set `proxy_url` explicitly.
//!
//! Clients are cached per (ignore_tls, max_redirects, proxy, client TLS
//! material) option set.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, Url};
use serde_json::Value;

use super::http_auth;
use super::tls::{self, CertSummary};
use super::util::{self, elapsed_ms};
use crate::db::models::{CheckOutcome, HeartbeatStatus, Monitor, MonitorType};

/// Methods offered in the monitor form.
pub const HTTP_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
/// Values of `monitors.http_body_encoding`.
pub const BODY_ENCODINGS: &[&str] = &["json", "form", "raw"];
/// Values of `monitors.json_operator`.
pub const JSON_OPERATORS: &[&str] = &["==", "!=", "contains", "<", "<=", ">", ">="];

/// Response bodies are read up to this many bytes (keyword / JSON checks).
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
/// Most redirects ever followed, whatever the monitor asks for.
const MAX_REDIRECTS_CAP: u32 = 5;

// ---------------------------------------------------------------------------
// Accepted status codes
// ---------------------------------------------------------------------------

/// Parsed `accepted_status_codes`: inclusive ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusCodes(Vec<(u16, u16)>);

impl StatusCodes {
    pub fn contains(&self, code: u16) -> bool {
        self.0.iter().any(|&(lo, hi)| (lo..=hi).contains(&code))
    }
}

/// Parse an accepted-status-codes spec such as `200-299,301,418` or `2xx,304`.
/// Empty means the default `200-399`.
pub fn parse_status_codes(spec: &str) -> Result<StatusCodes, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok(StatusCodes(vec![(200, 399)]));
    }
    let code = |s: &str| -> Result<u16, String> {
        let s = s.trim();
        match s.parse::<u16>() {
            Ok(c) if (100..=599).contains(&c) => Ok(c),
            _ => Err(format!("\"{s}\" is not a status code (100-599)")),
        }
    };
    let mut ranges = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let lower = part.to_ascii_lowercase();
        if lower.len() == 3 && lower.ends_with("xx") {
            let class = code(&format!("{}00", &lower[..1]))?;
            ranges.push((class, class + 99));
        } else if let Some((lo, hi)) = part.split_once('-') {
            let (lo, hi) = (code(lo)?, code(hi)?);
            if lo > hi {
                return Err(format!("range \"{part}\" is reversed"));
            }
            ranges.push((lo, hi));
        } else {
            let c = code(part)?;
            ranges.push((c, c));
        }
    }
    if ranges.is_empty() {
        return Err("no status codes given".into());
    }
    Ok(StatusCodes(ranges))
}

// ---------------------------------------------------------------------------
// Headers / body
// ---------------------------------------------------------------------------

/// Parse the `http_headers` JSON object into a header map. Non-string
/// values (numbers, booleans) are stringified.
pub fn parse_headers(json: &str) -> Result<HeaderMap, String> {
    let mut map = HeaderMap::new();
    if json.trim().is_empty() {
        return Ok(map);
    }
    let value: Value =
        serde_json::from_str(json).map_err(|e| format!("headers are not valid JSON: {e}"))?;
    let Value::Object(obj) = value else {
        return Err("headers must be a JSON object like {\"X-Token\": \"abc\"}".into());
    };
    for (name, value) in obj {
        let text = match value {
            Value::String(s) => s,
            Value::Null => continue,
            other => other.to_string(),
        };
        let name = HeaderName::from_bytes(name.trim().as_bytes())
            .map_err(|_| format!("invalid header name \"{name}\""))?;
        let value = HeaderValue::from_str(&text)
            .map_err(|_| format!("invalid value for header \"{name}\""))?;
        map.append(name, value);
    }
    Ok(map)
}

fn apply_body(
    req: reqwest::RequestBuilder,
    monitor: &Monitor,
    has_content_type: bool,
) -> Result<reqwest::RequestBuilder, String> {
    let Some(body) = monitor
        .http_body
        .as_deref()
        .filter(|b| !b.trim().is_empty())
    else {
        return Ok(req);
    };
    match monitor.http_body_encoding.as_str() {
        "form" => {
            if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(body) {
                let fields: BTreeMap<String, String> = obj
                    .into_iter()
                    .map(|(k, v)| match v {
                        Value::String(s) => (k, s),
                        other => (k, other.to_string()),
                    })
                    .collect();
                return Ok(req.form(&fields));
            }
            let req = if has_content_type {
                req
            } else {
                req.header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            };
            Ok(req.body(body.to_owned()))
        }
        "raw" => Ok(req.body(body.to_owned())),
        // "json" (default)
        _ => {
            serde_json::from_str::<Value>(body)
                .map_err(|e| format!("Request body is not valid JSON: {e}"))?;
            let req = if has_content_type {
                req
            } else {
                req.header(CONTENT_TYPE, "application/json")
            };
            Ok(req.body(body.to_owned()))
        }
    }
}

// ---------------------------------------------------------------------------
// Client cache
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClientKey {
    ignore_tls: bool,
    max_redirects: u32,
    proxy: Option<String>,
    /// The monitor sends custom headers (see [`redirect_policy`]).
    custom_headers: bool,
    /// Digest of the mTLS certificate, key and CA ([`http_auth::mtls_fingerprint`]).
    mtls: Option<String>,
}

impl ClientKey {
    fn for_monitor(monitor: &Monitor) -> Self {
        Self {
            ignore_tls: monitor.ignore_tls,
            max_redirects: u32::try_from(monitor.max_redirects.max(0))
                .unwrap_or(MAX_REDIRECTS_CAP)
                .min(MAX_REDIRECTS_CAP),
            proxy: monitor
                .proxy_url
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string),
            custom_headers: monitor
                .http_headers
                .as_deref()
                .is_some_and(|h| parse_headers(h).is_ok_and(|m| !m.is_empty())),
            mtls: http_auth::mtls_fingerprint(monitor),
        }
    }
}

/// The monitor's client certificate, key and extra CA (PEM), for mTLS.
fn mtls_material(monitor: &Monitor) -> Option<(&str, &str, Option<&str>)> {
    (monitor.auth_method == "mtls").then(|| {
        (
            monitor.tls_client_cert.as_deref().unwrap_or_default(),
            monitor.tls_client_key.as_deref().unwrap_or_default(),
            monitor.tls_ca_cert.as_deref(),
        )
    })
}

/// Most HTTP clients kept at once.
const MAX_CLIENTS: usize = 256;
/// Clients unused for this long are dropped.
const CLIENT_IDLE: Duration = Duration::from_secs(30 * 60);

/// Built clients by settings. Bounded: every mTLS certificate (and proxy)
/// makes its own client, so entries unused for [`CLIENT_IDLE`] are dropped
/// and at most [`MAX_CLIENTS`] are kept (the least recently used goes
/// first). A dropped client only closes its idle connections; requests in
/// flight hold their own handle.
struct ClientCache<V> {
    entries: HashMap<ClientKey, (V, Instant)>,
}

impl<V: Clone> ClientCache<V> {
    fn get(&mut self, key: &ClientKey, now: Instant) -> Option<V> {
        let (value, used) = self.entries.get_mut(key)?;
        *used = now;
        Some(value.clone())
    }

    fn insert(&mut self, key: ClientKey, value: V, now: Instant) {
        self.entries
            .retain(|_, (_, used)| now.duration_since(*used) < CLIENT_IDLE);
        if self.entries.len() >= MAX_CLIENTS
            && !self.entries.contains_key(&key)
            && let Some(lru) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone())
        {
            self.entries.remove(&lru);
        }
        self.entries.insert(key, (value, now));
    }
}

/// The error a redirect hop that went past the limit ends with.
const TOO_MANY_REDIRECTS: &str = "too many redirects";

/// Why the redirect from `previous` (every URL requested so far, the
/// original first) to `next` must not be followed, if it mustn't. A redirect
/// is refused when:
/// - the limit is used up,
/// - it leads to a link-local or cloud metadata address (names are checked
///   when they are resolved), or
/// - the monitor sends custom headers and the redirect leaves the host or
///   drops from https to http: reqwest only strips `Authorization` and
///   cookies there, so an `X-Api-Key` would be handed to whoever the server
///   (or an open redirect on it) names.
fn redirect_refusal(
    previous: &[Url],
    next: &Url,
    limit: usize,
    custom_headers: bool,
) -> Option<String> {
    if previous.len() > limit {
        return Some(TOO_MANY_REDIRECTS.into());
    }
    if let Some(host) = next.host_str()
        && let Err(forbidden) = crate::kit::net::check_host(host)
    {
        return Some(forbidden.to_string());
    }
    let from = previous.last()?;
    let leaves = from.host_str() != next.host_str()
        || (from.scheme() == "https" && next.scheme() != "https");
    (custom_headers && leaves).then(|| {
        format!(
            "redirected to {}, which gets none of the monitor's custom headers",
            next.host_str().unwrap_or("another host")
        )
    })
}

fn redirect_policy(max_redirects: u32, custom_headers: bool) -> reqwest::redirect::Policy {
    if max_redirects == 0 {
        return reqwest::redirect::Policy::none();
    }
    reqwest::redirect::Policy::custom(move |attempt| {
        match redirect_refusal(
            attempt.previous(),
            attempt.url(),
            max_redirects as usize,
            custom_headers,
        ) {
            Some(why) => attempt.error(why),
            None => attempt.follow(),
        }
    })
}

fn client_for(
    key: &ClientKey,
    mtls: Option<(&str, &str, Option<&str>)>,
) -> Result<reqwest::Client, String> {
    static CLIENTS: OnceLock<Mutex<ClientCache<reqwest::Client>>> = OnceLock::new();
    let cache = CLIENTS.get_or_init(|| {
        Mutex::new(ClientCache {
            entries: HashMap::new(),
        })
    });
    if let Ok(mut map) = cache.lock()
        && let Some(client) = map.get(key, Instant::now())
    {
        return Ok(client);
    }

    let redirect = redirect_policy(key.max_redirects, key.custom_headers);
    let mut builder = crate::tls_roots::http_client()
        .user_agent(crate::server::USER_AGENT)
        .redirect(redirect)
        // Link-local and cloud metadata addresses are refused when the
        // connection is made, redirects included. Private and loopback
        // addresses stay reachable: monitoring the LAN is the point.
        .dns_resolver(Arc::new(crate::kit::net::GuardedResolver))
        .tls_info(true)
        .danger_accept_invalid_certs(key.ignore_tls)
        .connect_timeout(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(60));
    builder = match &key.proxy {
        Some(url) => {
            builder.proxy(reqwest::Proxy::all(url).map_err(|e| format!("Invalid proxy URL: {e}"))?)
        }
        None => builder.no_proxy(),
    };
    if key.mtls.is_some()
        && let Some((cert, private_key, ca)) = mtls
    {
        builder =
            http_auth::apply_mtls(builder, cert, private_key, ca).map_err(|e| e.to_string())?;
    }
    let client = builder
        .build()
        .map_err(|e| format!("Could not build HTTP client: {}", util::root_cause(&e)))?;
    if let Ok(mut map) = cache.lock() {
        map.insert(key.clone(), client.clone(), Instant::now());
    }
    Ok(client)
}

// ---------------------------------------------------------------------------
// Check
// ---------------------------------------------------------------------------

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    check_inner(monitor)
        .await
        .unwrap_or_else(CheckOutcome::down)
}

/// `Err` = configuration problem / early failure, reported as Down.
async fn check_inner(monitor: &Monitor) -> Result<CheckOutcome, String> {
    let codes = parse_status_codes(&monitor.accepted_status_codes)
        .map_err(|e| format!("Invalid accepted status codes: {e}"))?;
    let url = parse_url(&monitor.target)?;
    // A literal address never reaches the resolver, so it is checked here.
    if let Some(host) = url.host_str() {
        crate::kit::net::check_host(host).map_err(|e| format!("Refused: {e}"))?;
    }
    let method_name = monitor.http_method.trim().to_ascii_uppercase();
    let method = if method_name.is_empty() {
        Method::GET
    } else {
        Method::from_bytes(method_name.as_bytes())
            .map_err(|_| format!("Invalid HTTP method \"{method_name}\""))?
    };
    let key = ClientKey::for_monitor(monitor);
    let client = client_for(&key, mtls_material(monitor))?;
    let timeout = super::timeout_of(monitor);

    let headers = match monitor.http_headers.as_deref() {
        Some(h) => parse_headers(h).map_err(|e| format!("Invalid headers: {e}"))?,
        None => HeaderMap::new(),
    };
    let has_content_type = headers.contains_key(CONTENT_TYPE);
    let build = |bearer: Option<HeaderValue>| {
        let mut headers = headers.clone();
        // The OAuth2 token wins over a hand-written Authorization header.
        if let Some(value) = bearer {
            headers.insert(AUTHORIZATION, value);
        }
        let mut req = client
            .request(method.clone(), url.clone())
            .timeout(timeout)
            .headers(headers);
        if monitor.auth_method == "basic"
            && let Some(user) = monitor.basic_auth_user.as_deref().filter(|u| !u.is_empty())
        {
            req = req.basic_auth(user, monitor.basic_auth_pass.as_deref());
        }
        apply_body(req, monitor, has_content_type)
    };

    // OAuth2: a cached token is reused until shortly before it expires. The
    // token endpoint is another server: no redirects, no client certificate.
    let token_client = || {
        client_for(
            &ClientKey {
                max_redirects: 0,
                mtls: None,
                ..key.clone()
            },
            None,
        )
    };
    let oauth = monitor.auth_method == "oauth2";
    let (bearer, token_cached) = if oauth {
        let (value, cached) = http_auth::bearer(monitor, &token_client()?, timeout).await?;
        (Some(value), cached)
    } else {
        (None, false)
    };

    let mut start = Instant::now();
    let mut sent = build(bearer)?.send().await;
    if token_cached
        && let Ok(resp) = &sent
        && resp.status() == reqwest::StatusCode::UNAUTHORIZED
    {
        // The token may have been revoked or rotated before its expiry:
        // fetch a new one and try once more.
        http_auth::forget_token(monitor.id);
        let (value, _) = http_auth::bearer(monitor, &token_client()?, timeout).await?;
        start = Instant::now();
        sent = build(Some(value))?.send().await;
    }
    let resp = match sent {
        Ok(resp) => resp,
        Err(err) => return Ok(request_error(&err, monitor, &url, &key, timeout).await),
    };
    let cert = resp
        .extensions()
        .get::<reqwest::tls::TlsInfo>()
        .and_then(|info| info.peer_certificate())
        .and_then(tls::parse_cert);
    let expiry = cert.as_ref().and_then(CertSummary::expiry_info);
    let status = resp.status();
    let status_text = format_status(status);

    let finish = |status: HeartbeatStatus, ms: i64, message: String| CheckOutcome {
        status,
        response_time_ms: Some(ms),
        message: Some(message),
        expiry: expiry.clone(),
    };

    if !codes.contains(status.as_u16()) {
        let ms = elapsed_ms(start);
        let accepted = monitor.accepted_status_codes.trim();
        let hint = if accepted.is_empty() || accepted == "200-399" {
            String::new()
        } else {
            format!(" (accepted: {accepted})")
        };
        return Ok(finish(
            HeartbeatStatus::Down,
            ms,
            format!("{status_text}{hint}"),
        ));
    }

    match monitor.monitor_type {
        MonitorType::Keyword | MonitorType::JsonQuery => {
            let body = read_body_capped(resp, MAX_BODY_BYTES).await.map_err(|e| {
                format!(
                    "{status_text}, but reading the body failed: {}",
                    util::root_cause(&e)
                )
            })?;
            let ms = elapsed_ms(start);
            let verdict = if monitor.monitor_type == MonitorType::Keyword {
                evaluate_keyword(
                    &body,
                    monitor.keyword.as_deref().unwrap_or(""),
                    monitor.keyword_invert,
                )
            } else {
                evaluate_json(
                    &body,
                    monitor.json_path.as_deref().unwrap_or(""),
                    &monitor.json_operator,
                    monitor.json_expected.as_deref(),
                )
            };
            Ok(match verdict {
                Ok(msg) => finish(HeartbeatStatus::Up, ms, msg),
                Err(msg) => finish(HeartbeatStatus::Down, ms, msg),
            })
        }
        _ => Ok(finish(HeartbeatStatus::Up, elapsed_ms(start), status_text)),
    }
}

fn parse_url(target: &str) -> Result<Url, String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("No URL configured".into());
    }
    let url = Url::parse(target)
        .map_err(|e| format!("Invalid URL \"{}\": {e}", util::truncate(target, 80)))?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        other => Err(format!(
            "Unsupported URL scheme \"{other}\" (use http or https)"
        )),
    }
}

/// "HTTP 503 Service Unavailable".
fn format_status(status: reqwest::StatusCode) -> String {
    match status.canonical_reason() {
        Some(reason) => format!("HTTP {} {reason}", status.as_u16()),
        None => format!("HTTP {}", status.as_u16()),
    }
}

async fn read_body_capped(
    mut resp: reqwest::Response,
    cap: usize,
) -> Result<String, reqwest::Error> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        let room = cap.saturating_sub(buf.len());
        if chunk.len() >= room {
            buf.extend_from_slice(&chunk[..room]);
            break;
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Turn a failed request into a clear Down outcome. For TLS verification
/// failures the certificate is fetched again without verification so the
/// message (and expiry info) can say *why*, e.g. "expired 3 days ago".
async fn request_error(
    err: &reqwest::Error,
    monitor: &Monitor,
    url: &Url,
    key: &ClientKey,
    timeout: Duration,
) -> CheckOutcome {
    let mut outcome = describe_request_error(err, monitor, url, key, timeout).await;
    // reqwest errors embed the full request URL; the message is stored and sent in
    // notifications, so it must never carry credentials or query-string tokens.
    if let Some(message) = outcome.message.take() {
        outcome.message = Some(scrub_url(&message, url));
    }
    outcome
}

/// Replace every occurrence of `url` in `message` with the URL minus userinfo,
/// query and fragment.
fn scrub_url(message: &str, url: &Url) -> String {
    let mut public = url.clone();
    let _ = public.set_username("");
    let _ = public.set_password(None);
    public.set_query(None);
    public.set_fragment(None);
    message.replace(url.as_str(), public.as_str())
}

async fn describe_request_error(
    err: &reqwest::Error,
    monitor: &Monitor,
    url: &Url,
    key: &ClientKey,
    timeout: Duration,
) -> CheckOutcome {
    let chain = util::full_chain(err);
    if err.is_timeout() {
        return CheckOutcome::down(format!("Timed out after {}", util::format_secs(timeout)));
    }
    if err.is_redirect() {
        let cause = util::root_cause(err);
        return CheckOutcome::down(if cause == TOO_MANY_REDIRECTS {
            format!("Too many redirects (limit {}): {cause}", key.max_redirects)
        } else {
            format!("Redirect not followed: {cause}")
        });
    }
    if chain.contains(&crate::kit::net::Forbidden.to_string()) {
        return CheckOutcome::down(format!("Refused: {}", util::root_cause(err)));
    }
    if url.scheme() == "https"
        && !monitor.ignore_tls
        && let Some(reason) = tls::classify_tls_error(&chain)
    {
        let probe_key = ClientKey {
            ignore_tls: true,
            max_redirects: 0,
            proxy: key.proxy.clone(),
            custom_headers: false,
            mtls: None,
        };
        let cert = probe_cert(&probe_key, url, timeout.min(Duration::from_secs(10))).await;
        let now = util::now_unix();
        let mut outcome = match &cert {
            Some(c) if c.is_expired(now) => CheckOutcome::down(c.expired_message(now)),
            _ => CheckOutcome::down(reason),
        };
        outcome.expiry = cert.as_ref().and_then(CertSummary::expiry_info);
        return outcome;
    }
    if chain.contains("dns error") {
        let host = url.host_str().unwrap_or_default();
        return CheckOutcome::down(format!(
            "DNS lookup failed for {host}: {}",
            util::root_cause(err)
        ));
    }
    if err.is_connect() {
        return CheckOutcome::down(format!("Connection failed: {}", util::root_cause(err)));
    }
    if err.is_builder() {
        return CheckOutcome::down(format!("Invalid request: {}", util::root_cause(err)));
    }
    CheckOutcome::down(util::root_cause(err))
}

/// Fetch the leaf certificate of an `https://` URL without verifying it
/// (HEAD request, no redirects, no proxy). Used to explain TLS failures.
pub(crate) async fn probe_cert_unverified(url: &Url, timeout: Duration) -> Option<CertSummary> {
    let key = ClientKey {
        ignore_tls: true,
        max_redirects: 0,
        proxy: None,
        custom_headers: false,
        mtls: None,
    };
    probe_cert(&key, url, timeout).await
}

async fn probe_cert(key: &ClientKey, url: &Url, timeout: Duration) -> Option<CertSummary> {
    let client = client_for(key, None).ok()?;
    let resp = client
        .head(url.clone())
        .timeout(timeout)
        .send()
        .await
        .ok()?;
    let der = resp
        .extensions()
        .get::<reqwest::tls::TlsInfo>()?
        .peer_certificate()?;
    tls::parse_cert(der)
}

// ---------------------------------------------------------------------------
// Keyword / JSON evaluation (pure)
// ---------------------------------------------------------------------------

// What a monitor reads from the answer stays out of its messages. They are
// stored and shown to everyone who can see the monitor, sent by notifications
// and read by assistants, while a monitor can be pointed at any URL the
// server can reach. So: no body excerpts, and JSON text is described, not
// quoted. Numbers and booleans are shown, so "queue length > 5" still reads
// well.

/// How a JSON value appears in a message: numbers, booleans and null as they
/// are, everything else by kind and size.
fn describe_json(v: &Value) -> String {
    match v {
        Value::Null | Value::Bool(_) | Value::Number(_) => v.to_string(),
        Value::String(s) => format!("text ({} characters)", s.chars().count()),
        Value::Array(a) => format!("a list of {}", a.len()),
        Value::Object(o) => format!("an object with {} keys", o.len()),
    }
}

/// The value itself, when it is safe to show.
fn scalar_json(v: &Value) -> Option<String> {
    matches!(v, Value::Null | Value::Bool(_) | Value::Number(_)).then(|| v.to_string())
}

/// `Ok(up message)` / `Err(down message)`. Case-sensitive substring match.
pub fn evaluate_keyword(body: &str, keyword: &str, invert: bool) -> Result<String, String> {
    if keyword.is_empty() {
        return Err("No keyword configured".into());
    }
    let shown = util::truncate(keyword, 40);
    match (body.find(keyword), invert) {
        (Some(_), false) => Ok(format!("Keyword \"{shown}\" found")),
        (None, true) => Ok(format!("Keyword \"{shown}\" absent, as expected")),
        (None, false) => {
            if body.trim().is_empty() {
                Err(format!("Keyword \"{shown}\" not found — body is empty"))
            } else {
                Err(format!("Keyword \"{shown}\" not found"))
            }
        }
        (Some(_), true) => Err(format!("Keyword \"{shown}\" found but should be absent")),
    }
}

/// Accept RFC 9535 paths (`$.a.b[0]`) and the bare shorthand `a.b` / `.a.b` / `[0]`.
pub fn normalize_json_path(path: &str) -> String {
    let p = path.trim();
    if p.starts_with('$') {
        p.to_string()
    } else if p.starts_with('.') || p.starts_with('[') {
        format!("${p}")
    } else {
        format!("$.{p}")
    }
}

/// Evaluate a JSON query against a response body.
/// `Ok(up message)` / `Err(down message)`. With no expected value the check
/// passes when the path matches anything.
pub fn evaluate_json(
    body: &str,
    path: &str,
    operator: &str,
    expected: Option<&str>,
) -> Result<String, String> {
    if path.trim().is_empty() {
        return Err("No JSON path configured".into());
    }
    let value: Value = serde_json::from_str(body).map_err(|e| {
        if body.trim().is_empty() {
            "Response is not JSON — body is empty".to_string()
        } else {
            format!("Response is not valid JSON ({e})")
        }
    })?;
    let path = normalize_json_path(path);
    let compiled = serde_json_path::JsonPath::parse(&path)
        .map_err(|e| format!("Invalid JSON path {path}: {e}"))?;
    let nodes = compiled.query(&value);
    let actual = nodes
        .first()
        .ok_or_else(|| format!("JSON path {path} matched nothing"))?;

    let Some(expected) = expected.filter(|e| !e.is_empty()) else {
        return Ok(match scalar_json(actual) {
            Some(value) => format!("{path} = {value}"),
            None => format!("{path} matched"),
        });
    };
    let operator = operator.trim();
    if json_compare(operator, actual, expected)? {
        Ok(match scalar_json(actual) {
            Some(value) => format!("{path} {operator} {expected} (actual: {value})"),
            None => format!("{path} {operator} {expected}"),
        })
    } else {
        Err(format!(
            "{path} is {}, expected {operator} {expected}",
            describe_json(actual)
        ))
    }
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn value_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn loose_eq(actual: &Value, expected: &str) -> bool {
    match (value_f64(actual), expected.trim().parse::<f64>()) {
        (Some(a), Ok(b)) => a == b,
        _ => value_text(actual) == expected,
    }
}

/// Compare `actual` with the user's `expected` text. Numeric when both sides
/// parse as numbers; otherwise string comparison. `contains` on an array
/// tests membership, on anything else a substring.
pub fn json_compare(operator: &str, actual: &Value, expected: &str) -> Result<bool, String> {
    match operator {
        "==" | "" => Ok(loose_eq(actual, expected)),
        "!=" => Ok(!loose_eq(actual, expected)),
        "contains" => Ok(match actual {
            Value::Array(items) => items.iter().any(|i| loose_eq(i, expected)),
            other => value_text(other).contains(expected),
        }),
        "<" | "<=" | ">" | ">=" => {
            let (Some(a), Ok(b)) = (value_f64(actual), expected.trim().parse::<f64>()) else {
                return Err(format!(
                    "Cannot compare {} {operator} {expected}: both sides must be numbers",
                    describe_json(actual)
                ));
            };
            Ok(match operator {
                "<" => a < b,
                "<=" => a <= b,
                ">" => a > b,
                _ => a >= b,
            })
        }
        other => Err(format!("Unknown JSON operator \"{other}\"")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn client_cache_drops_idle_clients_and_stays_bounded() {
        let key = |n: usize| ClientKey {
            ignore_tls: false,
            max_redirects: 0,
            proxy: None,
            custom_headers: false,
            mtls: Some(n.to_string()),
        };
        let mut cache = ClientCache {
            entries: HashMap::new(),
        };
        let t0 = Instant::now();
        cache.insert(key(0), 0, t0);
        cache.insert(key(1), 1, t0);
        // Key 0 stays in use; key 1 goes idle and is dropped on the next insert.
        let later = t0 + CLIENT_IDLE;
        assert_eq!(cache.get(&key(0), later - Duration::from_secs(1)), Some(0));
        cache.insert(key(2), 2, later);
        assert_eq!((cache.get(&key(1), later), cache.entries.len()), (None, 2));
        for n in 3..3 + MAX_CLIENTS {
            cache.insert(key(n), n, later + Duration::from_secs(n as u64));
        }
        assert_eq!(cache.entries.len(), MAX_CLIENTS);
        // The least recently used made room; the newest is kept.
        assert_eq!(cache.get(&key(0), later), None);
        assert!(cache.get(&key(2 + MAX_CLIENTS), later).is_some());
    }

    #[test]
    fn error_messages_never_carry_url_credentials() {
        let url = Url::parse("https://user:hunter2@api.example.com/health?token=abc#x").unwrap();
        let message = format!("error sending request for url ({url})");
        let scrubbed = scrub_url(&message, &url);
        assert_eq!(
            scrubbed,
            "error sending request for url (https://api.example.com/health)"
        );
    }

    #[test]
    fn status_codes_ranges_and_singles() {
        let c = parse_status_codes("200-299, 301,418").unwrap();
        assert!(c.contains(200) && c.contains(299) && c.contains(301) && c.contains(418));
        assert!(!c.contains(300) && !c.contains(302) && !c.contains(500));
    }

    #[test]
    fn status_codes_class_shorthand() {
        let c = parse_status_codes("2xx,304").unwrap();
        assert!(c.contains(204) && c.contains(304) && !c.contains(301));
    }

    #[test]
    fn status_codes_empty_is_default() {
        let c = parse_status_codes("  ").unwrap();
        assert!(c.contains(200) && c.contains(399) && !c.contains(400));
    }

    #[test]
    fn status_codes_reject_garbage() {
        assert!(parse_status_codes("abc").is_err());
        assert!(parse_status_codes("299-200").is_err());
        assert!(parse_status_codes("700").is_err());
        assert!(parse_status_codes(",").is_err());
    }

    #[test]
    fn headers_parse_object() {
        let h = parse_headers(r#"{"X-A": "1", "X-B": 2, "X-C": null}"#).unwrap();
        assert_eq!(h.get("x-a").unwrap(), "1");
        assert_eq!(h.get("x-b").unwrap(), "2");
        assert!(h.get("x-c").is_none());
        assert!(parse_headers("[1]").is_err());
        assert!(parse_headers("{").is_err());
        assert!(parse_headers(r#"{"bad name": "x"}"#).is_err());
        assert!(parse_headers("").unwrap().is_empty());
    }

    #[test]
    fn keyword_found_and_missing() {
        assert_eq!(
            evaluate_keyword("all OK here", "OK", false),
            Ok("Keyword \"OK\" found".into())
        );
        let err = evaluate_keyword("<html>\n  <body>fail</body>", "OK", false).unwrap_err();
        assert_eq!(err, "Keyword \"OK\" not found");
    }

    #[test]
    fn keyword_is_case_sensitive() {
        assert!(evaluate_keyword("all ok", "OK", false).is_err());
    }

    #[test]
    fn keyword_invert() {
        assert!(evaluate_keyword("fine", "ERROR", true).is_ok());
        let err = evaluate_keyword("status: ERROR occurred", "ERROR", true).unwrap_err();
        assert_eq!(err, "Keyword \"ERROR\" found but should be absent");
    }

    #[test]
    fn messages_never_quote_the_response() {
        // A monitor can be pointed at any URL the server can reach, and its
        // messages are shown to viewers and sent by notifications.
        let secret = "TOPSECRET-token-from-an-internal-service";
        let body = format!("{secret} and some more text");
        let err = evaluate_keyword(&body, "y", false).unwrap_err();
        assert!(!err.contains("TOPSECRET"), "{err}");
        let err = evaluate_keyword(&body, "and", true).unwrap_err();
        assert!(!err.contains("TOPSECRET"), "{err}");
        let err = evaluate_json(&body, "$.a", "==", Some("1")).unwrap_err();
        assert!(!err.contains("TOPSECRET"), "{err}");

        let json = format!(r#"{{"token":"{secret}","n":7,"on":true,"list":["{secret}"]}}"#);
        for path in ["$.token", "$.list", "$"] {
            for expected in [None, Some("x")] {
                let (Ok(msg) | Err(msg)) = evaluate_json(&json, path, "==", expected);
                assert!(!msg.contains("TOPSECRET"), "{path}: {msg}");
            }
        }
        let err = evaluate_json(&json, "$.token", "<", Some("1")).unwrap_err();
        assert!(!err.contains("TOPSECRET"), "{err}");
        // Numbers and booleans still read well.
        assert_eq!(
            evaluate_json(&json, "$.n", "==", None),
            Ok("$.n = 7".into())
        );
        assert_eq!(
            evaluate_json(&json, "$.on", "==", Some("false")),
            Err("$.on is true, expected == false".into())
        );
    }

    #[test]
    fn json_path_shorthand() {
        assert_eq!(normalize_json_path("a.b"), "$.a.b");
        assert_eq!(normalize_json_path("$.a"), "$.a");
        assert_eq!(normalize_json_path(".a"), "$.a");
        assert_eq!(normalize_json_path("[0].a"), "$[0].a");
    }

    #[test]
    fn json_compare_operators() {
        assert_eq!(json_compare("==", &json!("ok"), "ok"), Ok(true));
        assert_eq!(json_compare("==", &json!(5), "5.0"), Ok(true));
        assert_eq!(json_compare("==", &json!(true), "true"), Ok(true));
        assert_eq!(json_compare("!=", &json!("ok"), "down"), Ok(true));
        assert_eq!(json_compare(">", &json!(10), "5"), Ok(true));
        assert_eq!(json_compare("<=", &json!("3"), "3"), Ok(true));
        assert_eq!(json_compare(">=", &json!(2.5), "3"), Ok(false));
        assert_eq!(
            json_compare("contains", &json!("hello world"), "lo w"),
            Ok(true)
        );
        assert_eq!(json_compare("contains", &json!(["a", "b"]), "b"), Ok(true));
        assert_eq!(json_compare("contains", &json!([1, 2]), "3"), Ok(false));
        assert!(json_compare("<", &json!("abc"), "1").is_err());
        assert!(json_compare("~=", &json!(1), "1").is_err());
    }

    #[test]
    fn json_evaluate_messages() {
        let body = r#"{"status":"degraded","count":5,"items":["a"]}"#;
        assert_eq!(
            evaluate_json(body, "status", "==", Some("ok")),
            Err("$.status is text (8 characters), expected == ok".into())
        );
        assert_eq!(
            evaluate_json(body, "$.count", ">", Some("1")),
            Ok("$.count > 1 (actual: 5)".into())
        );
        assert_eq!(
            evaluate_json(body, "$.missing", "==", Some("x")),
            Err("JSON path $.missing matched nothing".into())
        );
        assert_eq!(
            evaluate_json(body, "$.items", "==", None),
            Ok("$.items matched".into())
        );
        assert!(
            evaluate_json("not json", "$.a", "==", Some("1"))
                .unwrap_err()
                .starts_with("Response is not valid JSON")
        );
        assert!(
            evaluate_json(body, "$[", "==", Some("1"))
                .unwrap_err()
                .starts_with("Invalid JSON path")
        );
    }

    #[test]
    fn client_key_normalises_options() {
        let mut m = crate::monitor::checkers::test_support::monitor(MonitorType::Http, "http://x");
        m.max_redirects = -3;
        m.proxy_url = Some("  ".into());
        let k = ClientKey::for_monitor(&m);
        assert_eq!(k.max_redirects, 0);
        assert_eq!(k.proxy, None);
        m.max_redirects = 1000;
        assert_eq!(ClientKey::for_monitor(&m).max_redirects, MAX_REDIRECTS_CAP);
    }

    #[test]
    fn invalid_proxy_is_reported() {
        let key = ClientKey {
            ignore_tls: false,
            max_redirects: 1,
            proxy: Some("::not a url".into()),
            custom_headers: false,
            mtls: None,
        };
        assert!(
            client_for(&key, None)
                .unwrap_err()
                .starts_with("Invalid proxy URL")
        );
    }
}

/// Tests against a local axum server (no internet).
#[cfg(test)]
mod server_tests {
    use std::net::SocketAddr;

    use axum::Router;
    use axum::http::{HeaderMap as AxHeaders, StatusCode};
    use axum::response::{IntoResponse, Redirect};
    use axum::routing::{any, get};

    use super::*;
    use crate::monitor::checkers::test_support::monitor;

    async fn serve() -> SocketAddr {
        let app = Router::new()
            .route("/ok", get(|| async { "hello world OK" }))
            .route(
                "/status/{code}",
                get(|axum::extract::Path(code): axum::extract::Path<u16>| async move {
                    StatusCode::from_u16(code).unwrap_or(StatusCode::IM_A_TEAPOT)
                }),
            )
            .route(
                "/json",
                get(|| async {
                    axum::Json(serde_json::json!({"status": "ok", "count": 5, "items": ["a", "b"], "nested": {"v": 1.5}}))
                }),
            )
            .route("/redirect", get(|| async { Redirect::temporary("/ok") }))
            .route("/loop", get(|| async { Redirect::temporary("/loop") }))
            .route(
                "/headers",
                get(|h: AxHeaders| async move {
                    if h.get("x-test").is_some_and(|v| v == "yes") { StatusCode::OK } else { StatusCode::BAD_REQUEST }
                }),
            )
            .route(
                "/auth",
                get(|h: AxHeaders| async move {
                    // base64("user:pass") = dXNlcjpwYXNz
                    if h.get("authorization").is_some_and(|v| v == "Basic dXNlcjpwYXNz") {
                        StatusCode::OK
                    } else {
                        StatusCode::UNAUTHORIZED
                    }
                }),
            )
            .route(
                "/echo",
                any(|method: axum::http::Method, h: AxHeaders, body: String| async move {
                    let ct = h.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                    format!("{method} {ct} {body}").into_response()
                }),
            )
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    "late"
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        addr
    }

    fn http(addr: SocketAddr, path: &str) -> Monitor {
        monitor(MonitorType::Http, &format!("http://{addr}{path}"))
    }

    #[tokio::test]
    async fn plain_ok() {
        let addr = serve().await;
        let out = check(&http(addr, "/ok")).await;
        assert_eq!(out.status, HeartbeatStatus::Up);
        assert_eq!(out.message.as_deref(), Some("HTTP 200 OK"));
        assert!(out.response_time_ms.is_some());
        assert!(out.expiry.is_none());
    }

    #[tokio::test]
    async fn bad_status_is_down() {
        let addr = serve().await;
        let out = check(&http(addr, "/status/503")).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert_eq!(out.message.as_deref(), Some("HTTP 503 Service Unavailable"));
    }

    #[tokio::test]
    async fn custom_accepted_codes() {
        let addr = serve().await;
        let mut m = http(addr, "/status/503");
        m.accepted_status_codes = "500-599".into();
        assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
        let mut m = http(addr, "/status/204");
        m.accepted_status_codes = "200".into();
        let out = check(&m).await;
        assert_eq!(
            out.message.as_deref(),
            Some("HTTP 204 No Content (accepted: 200)")
        );
    }

    #[tokio::test]
    async fn redirects_followed_or_not() {
        let addr = serve().await;
        let out = check(&http(addr, "/redirect")).await;
        assert_eq!(out.message.as_deref(), Some("HTTP 200 OK"));

        let mut m = http(addr, "/redirect");
        m.max_redirects = 0;
        m.accepted_status_codes = "200-299".into();
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert_eq!(
            out.message.as_deref(),
            Some("HTTP 307 Temporary Redirect (accepted: 200-299)")
        );

        let mut m = http(addr, "/loop");
        m.max_redirects = 3;
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(
            out.message
                .unwrap()
                .starts_with("Too many redirects (limit 3)")
        );
    }

    #[tokio::test]
    async fn metadata_and_link_local_addresses_are_refused() {
        for target in [
            "http://169.254.169.254/latest/meta-data/",
            "http://[fd00:ec2::254]/",
            "http://[fe80::1]/",
            "http://[::ffff:169.254.169.254]/",
            "http://0.0.0.0:8080/",
            // The decimal spelling is normalised by the URL parser.
            "http://2852039166/",
        ] {
            let out = check(&monitor(MonitorType::Http, target)).await;
            assert_eq!(out.status, HeartbeatStatus::Down, "{target}");
            let msg = out.message.unwrap();
            assert!(
                msg.starts_with("Refused: the address is not allowed"),
                "{target}: {msg}"
            );
        }
        // The LAN and the machine itself are what monitors are for.
        let addr = serve().await;
        assert_eq!(check(&http(addr, "/ok")).await.status, HeartbeatStatus::Up);
    }

    #[tokio::test]
    async fn a_redirect_to_a_metadata_address_is_not_followed() {
        let app = Router::new().route(
            "/hop",
            get(|| async { Redirect::temporary("http://169.254.169.254/latest/meta-data/") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let out = check(&http(addr, "/hop")).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        let msg = out.message.unwrap();
        assert!(
            msg.starts_with("Redirect not followed: the address is not allowed"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn at_most_five_redirects_are_followed() {
        let addr = serve().await;
        let mut m = http(addr, "/loop");
        m.max_redirects = 30;
        let msg = check(&m).await.message.unwrap();
        assert!(msg.starts_with("Too many redirects (limit 5)"), "{msg}");
    }

    #[tokio::test]
    async fn custom_headers_are_not_sent_to_another_host() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(AtomicUsize::new(0));
        let hits = std::sync::Arc::clone(&seen);
        let app = Router::new()
            .route(
                "/seen",
                get(move |h: AxHeaders| async move {
                    if h.get("x-api-key").is_some() {
                        hits.fetch_add(1, Ordering::SeqCst);
                    }
                    "ok"
                }),
            )
            // Same host, other host (another name for the same machine).
            .route("/same", get(|| async { Redirect::temporary("/seen") }))
            .route(
                "/other",
                get(move || async move {
                    Redirect::temporary(&format!("http://localhost:{}/seen", addr.port()))
                }),
            );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut m = http(addr, "/same");
        m.http_headers = Some(r#"{"X-Api-Key": "secret"}"#.into());
        assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
        assert_eq!(seen.load(Ordering::SeqCst), 1, "kept on the same host");

        let mut m = http(addr, "/other");
        m.http_headers = Some(r#"{"X-Api-Key": "secret"}"#.into());
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        let msg = out.message.unwrap();
        assert!(
            msg.starts_with("Redirect not followed: redirected to localhost"),
            "{msg}"
        );
        assert_eq!(
            seen.load(Ordering::SeqCst),
            1,
            "never sent to the other host"
        );

        // Nothing to leak, nothing to stop.
        let m = http(addr, "/other");
        assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
    }

    #[test]
    fn redirect_rules() {
        let url = |s: &str| Url::parse(s).unwrap();
        let first = [url("https://a.example/")];
        // Own host (also to another path or port): fine, headers or not.
        assert_eq!(
            redirect_refusal(&first, &url("https://a.example:8443/x"), 5, true),
            None
        );
        // Another host: only without custom headers.
        let other = url("https://b.example/");
        assert_eq!(redirect_refusal(&first, &other, 5, false), None);
        assert!(redirect_refusal(&first, &other, 5, true).is_some());
        // A downgrade to http would put them on the wire.
        let plain = url("http://a.example/");
        assert_eq!(redirect_refusal(&first, &plain, 5, false), None);
        assert!(redirect_refusal(&first, &plain, 5, true).is_some());
        // Upgrading is fine.
        let http_first = [url("http://a.example/")];
        assert_eq!(
            redirect_refusal(&http_first, &url("https://a.example/"), 5, true),
            None
        );
        // Metadata addresses, whatever the headers.
        let meta = url("http://169.254.169.254/");
        assert!(redirect_refusal(&first, &meta, 5, false).is_some());
        assert!(redirect_refusal(&first, &url("http://[fe80::1]/"), 5, false).is_some());
        assert_eq!(
            redirect_refusal(&first, &url("http://10.0.0.5/"), 5, false),
            None
        );
        // The limit: the original plus five hops.
        let six: Vec<Url> = (0..6).map(|_| url("https://a.example/")).collect();
        assert_eq!(redirect_refusal(&six[..5], &six[0], 5, false), None);
        assert_eq!(
            redirect_refusal(&six, &six[0], 5, false).as_deref(),
            Some(TOO_MANY_REDIRECTS)
        );
    }

    #[tokio::test]
    async fn custom_headers_sent() {
        let addr = serve().await;
        let mut m = http(addr, "/headers");
        assert_eq!(check(&m).await.status, HeartbeatStatus::Down);
        m.http_headers = Some(r#"{"X-Test": "yes"}"#.into());
        assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
        m.http_headers = Some("{nope".into());
        assert!(
            check(&m)
                .await
                .message
                .unwrap()
                .starts_with("Invalid headers")
        );
    }

    #[tokio::test]
    async fn basic_auth() {
        let addr = serve().await;
        let mut m = http(addr, "/auth");
        assert_eq!(
            check(&m).await.message.as_deref(),
            Some("HTTP 401 Unauthorized")
        );
        m.basic_auth_user = Some("user".into());
        m.basic_auth_pass = Some("pass".into());
        // The method decides: leftover credentials with "none" are not sent.
        assert_eq!(
            check(&m).await.message.as_deref(),
            Some("HTTP 401 Unauthorized")
        );
        m.auth_method = "basic".into();
        assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
    }

    #[tokio::test]
    async fn keyword_checks() {
        let addr = serve().await;
        let mut m = monitor(MonitorType::Keyword, &format!("http://{addr}/ok"));
        m.keyword = Some("world".into());
        assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
        m.keyword = Some("missing".into());
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert_eq!(
            out.message.as_deref(),
            Some("Keyword \"missing\" not found")
        );
        m.keyword_invert = true;
        assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
    }

    #[tokio::test]
    async fn json_query_checks() {
        let addr = serve().await;
        let mut m = monitor(MonitorType::JsonQuery, &format!("http://{addr}/json"));
        m.json_path = Some("status".into());
        m.json_expected = Some("ok".into());
        assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
        m.json_path = Some("$.nested.v".into());
        m.json_operator = ">=".into();
        m.json_expected = Some("2".into());
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert_eq!(
            out.message.as_deref(),
            Some("$.nested.v is 1.5, expected >= 2")
        );
        m.json_path = Some("$.items".into());
        m.json_operator = "contains".into();
        m.json_expected = Some("b".into());
        assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
    }

    #[tokio::test]
    async fn bodies_and_methods() {
        let addr = serve().await;
        let mut m = monitor(MonitorType::Keyword, &format!("http://{addr}/echo"));
        m.http_method = "post".into();
        m.http_body = Some(r#"{"a":1}"#.into());
        m.keyword = Some(r#"POST application/json {"a":1}"#.into());
        assert_eq!(check(&m).await.status, HeartbeatStatus::Up, "json body");

        m.http_body_encoding = "form".into();
        m.http_body = Some(r#"{"a":"x y","b":2}"#.into());
        m.keyword = Some("POST application/x-www-form-urlencoded a=x+y&b=2".into());
        assert_eq!(check(&m).await.status, HeartbeatStatus::Up, "form body");

        m.http_body_encoding = "raw".into();
        m.http_method = "PUT".into();
        m.http_headers = Some(r#"{"Content-Type":"text/plain"}"#.into());
        m.http_body = Some("plain text".into());
        m.keyword = Some("PUT text/plain plain text".into());
        assert_eq!(check(&m).await.status, HeartbeatStatus::Up, "raw body");

        m.http_body_encoding = "json".into();
        m.http_body = Some("{broken".into());
        assert!(
            check(&m)
                .await
                .message
                .unwrap()
                .starts_with("Request body is not valid JSON")
        );
    }

    #[tokio::test]
    async fn timeout_reported() {
        let addr = serve().await;
        let mut m = http(addr, "/slow");
        m.timeout_seconds = 1;
        let out = check(&m).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert_eq!(out.message.as_deref(), Some("Timed out after 1s"));
    }

    #[tokio::test]
    async fn connection_refused_reported() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let out = check(&http(addr, "/")).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        assert!(out.message.unwrap().starts_with("Connection failed"));
    }

    #[tokio::test]
    async fn invalid_targets() {
        let out = check(&monitor(MonitorType::Http, "ftp://x")).await;
        assert!(out.message.unwrap().starts_with("Unsupported URL scheme"));
        let out = check(&monitor(MonitorType::Http, "")).await;
        assert_eq!(out.message.as_deref(), Some("No URL configured"));
    }

    #[tokio::test]
    #[ignore = "needs internet"]
    async fn live_https_reports_cert() {
        let out = check(&monitor(MonitorType::Http, "https://example.com/")).await;
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        let exp = out.expiry.expect("cert info");
        assert_eq!(exp.kind, crate::db::models::ExpiryKind::Cert);
        assert_eq!(exp.expires_at.len(), 19);
    }

    #[tokio::test]
    #[ignore = "needs internet"]
    async fn live_expired_cert_is_explained() {
        let out = check(&monitor(MonitorType::Http, "https://expired.badssl.com/")).await;
        assert_eq!(out.status, HeartbeatStatus::Down);
        let msg = out.message.unwrap();
        assert!(msg.starts_with("TLS: certificate expired"), "{msg}");
        assert!(out.expiry.is_some());

        let mut m = monitor(MonitorType::Http, "https://expired.badssl.com/");
        m.ignore_tls = true;
        assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
    }

    #[tokio::test]
    #[ignore = "needs internet"]
    async fn live_untrusted_cert_is_explained() {
        let out = check(&monitor(
            MonitorType::Http,
            "https://self-signed.badssl.com/",
        ))
        .await;
        let msg = out.message.unwrap();
        assert!(msg.starts_with("TLS: certificate is not trusted"), "{msg}");
        let out = check(&monitor(
            MonitorType::Http,
            "https://wrong.host.badssl.com/",
        ))
        .await;
        let msg = out.message.unwrap();
        assert!(msg.contains("hostname"), "{msg}");
        let out = check(&monitor(MonitorType::Http, "https://no-such-host.invalid/")).await;
        let msg = out.message.unwrap();
        assert!(
            msg.starts_with("DNS lookup failed for no-such-host.invalid"),
            "{msg}"
        );
    }
}
