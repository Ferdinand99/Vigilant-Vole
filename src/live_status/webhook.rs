//! Webhook addresses and the client that talks to them.
//!
//! A board's webhook is a secret (the id and token are a bearer credential)
//! and a destination somebody typed in, so both are handled strictly:
//!
//! * **Discord**: only `https://discord.com`, `discordapp.com`,
//!   `ptb.discord.com` or `canary.discord.com`, path `/api[/vN]/webhooks/{id}/{token}`.
//! * **Fluxer** is self-hostable, so its webhook URLs come from any
//!   instance: `https://<host>[/<prefix>]/webhooks/{id}/{token}` (the
//!   prefix is `/api` behind the stock reverse proxy). A host is used only
//!   after its discovery document (`/.well-known/fluxer`) says that it is a
//!   Fluxer instance and that the URL's prefix is one of its API endpoints
//!   ([`Client::confirm_instance`]); the address it resolves to must be a
//!   public one, which is checked again for every request and pinned for the
//!   connection. No proxy, no redirects.
//!
//! Errors returned from here never contain the URL or the token.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Hosts a Discord webhook may live on.
pub const DISCORD_HOSTS: &[&str] = &[
    "discord.com",
    "discordapp.com",
    "ptb.discord.com",
    "canary.discord.com",
];

/// How long a request waits for a rate limit to lift before it gives up
/// (the board tries again later).
const MAX_INLINE_WAIT: Duration = Duration::from_secs(10);
/// Longest wait any rate-limit header can make us honour.
const MAX_RATE_WAIT: Duration = Duration::from_secs(120);
/// How long one request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A positive discovery check is reused this long.
const DISCOVERY_TTL: Duration = Duration::from_secs(3600);
/// Largest discovery document read.
const DISCOVERY_MAX_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Discord,
    Fluxer,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Discord => "discord",
            Platform::Fluxer => "fluxer",
        }
    }

    pub fn parse(s: &str) -> Option<Platform> {
        match s {
            "discord" => Some(Platform::Discord),
            "fluxer" => Some(Platform::Fluxer),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Platform::Discord => "Discord",
            Platform::Fluxer => "Fluxer",
        }
    }
}

/// Why a webhook address was refused. Each maps to a message key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlError {
    /// Not an address.
    Invalid,
    /// Not `https`, or it carries a user name, password, query or fragment.
    Scheme,
    /// A host this platform's webhooks don't live on.
    Host,
    /// Not `…/webhooks/{id}/{token}`.
    Path,
}

impl UrlError {
    pub fn message_key(self) -> &'static str {
        match self {
            UrlError::Invalid | UrlError::Scheme | UrlError::Path => "live.err_url",
            UrlError::Host => "live.err_url_host",
        }
    }
}

/// A webhook address, taken apart. Its [`Debug`] output hides the token.
#[derive(Clone, PartialEq, Eq)]
pub struct Target {
    pub platform: Platform,
    /// `https://host[:port]` (no path).
    pub origin: String,
    /// The part of the path before `/webhooks`: "", "/api", "/api/v10" …
    pub prefix: String,
    pub id: String,
    pub token: String,
    host: String,
    port: u16,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("platform", &self.platform)
            .field("origin", &self.origin)
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

fn is_digits(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_digit())
}

fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `vNN`.
fn is_version(s: &str) -> bool {
    s.strip_prefix('v').is_some_and(|n| is_digits(n, 3))
}

impl Target {
    /// The webhook at `url` for `platform`, or why not. For Fluxer this
    /// checks the shape only: [`Client::confirm_instance`] does the rest.
    pub fn parse(platform: Platform, url: &str) -> Result<Target, UrlError> {
        Self::parse_with(platform, url, false)
    }

    /// `relaxed` (tests only): plain `http`, any host, IP literals.
    fn parse_with(platform: Platform, url: &str, relaxed: bool) -> Result<Target, UrlError> {
        let parsed = url::Url::parse(url.trim()).map_err(|_| UrlError::Invalid)?;
        let scheme_ok = parsed.scheme() == "https" || (relaxed && parsed.scheme() == "http");
        if !scheme_ok
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(UrlError::Scheme);
        }
        let host = parsed
            .host_str()
            .ok_or(UrlError::Invalid)?
            .to_ascii_lowercase();
        let segments: Vec<&str> = parsed.path_segments().ok_or(UrlError::Path)?.collect();
        // `…/webhooks/{id}/{token}`, with an optional trailing slash.
        let segments: Vec<&str> = match segments.as_slice() {
            [rest @ .., ""] => rest.to_vec(),
            all => all.to_vec(),
        };
        let [prefix @ .., "webhooks", id, token] = segments.as_slice() else {
            return Err(UrlError::Path);
        };
        if !is_digits(id, 30) || !is_token(token) {
            return Err(UrlError::Path);
        }
        let prefix_text = prefix.iter().fold(String::new(), |acc, s| acc + "/" + s);
        match platform {
            Platform::Discord => {
                if !relaxed && (!DISCORD_HOSTS.contains(&host.as_str()) || parsed.port().is_some())
                {
                    return Err(UrlError::Host);
                }
                let ok = matches!(prefix, ["api"] | ["api", _])
                    && prefix.get(1).is_none_or(|v| is_version(v));
                if !ok {
                    return Err(UrlError::Path);
                }
            }
            Platform::Fluxer => {
                // A Fluxer instance is named, never addressed by IP, and
                // never `localhost`.
                let literal = host.parse::<std::net::IpAddr>().is_ok() || host.starts_with('[');
                if !relaxed
                    && (literal
                        || !host.contains('.')
                        || host.ends_with('.')
                        || host == "localhost"
                        || host.ends_with(".localhost"))
                {
                    return Err(UrlError::Host);
                }
                if prefix
                    .iter()
                    .any(|s| s.is_empty() || s.contains(['%', '\\']))
                {
                    return Err(UrlError::Path);
                }
            }
        }
        let port = parsed.port_or_known_default().ok_or(UrlError::Invalid)?;
        let origin = match parsed.port() {
            Some(p) => format!("{}://{host}:{p}", parsed.scheme()),
            None => format!("{}://{host}", parsed.scheme()),
        };
        Ok(Target {
            platform,
            origin,
            prefix: prefix_text,
            id: (*id).to_string(),
            token: (*token).to_string(),
            host,
            port,
        })
    }

    /// What a rate limit belongs to: this webhook of this API.
    fn bucket_key(&self) -> String {
        format!("{}|{}", self.api_base(), self.id)
    }

    /// `{origin}{prefix}` (what Fluxer's `api_public` is compared to).
    pub fn api_base(&self) -> String {
        format!("{}{}", self.origin, self.prefix)
    }

    fn execute_url(&self) -> String {
        format!(
            "{}{}/webhooks/{}/{}",
            self.origin, self.prefix, self.id, self.token
        )
    }

    fn message_url(&self, message_id: &str) -> String {
        format!("{}/messages/{message_id}", self.execute_url())
    }

    /// Removes the token (and the whole address) from a message that may
    /// quote it.
    fn scrub(&self, text: &str) -> String {
        text.replace(&self.token, "…")
    }
}

/// How a request failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// Retrying will not help until somebody changes something (the
    /// webhook was deleted, the address is refused, the content is invalid).
    Permanent(String),
    /// Might work next time (network, 5xx).
    Transient(String),
    /// The platform asks us to wait this long.
    RateLimited(Duration),
    /// The message to edit or delete does not exist any more.
    MessageGone,
}

impl Failure {
    /// A line for the board's error column and the logs.
    pub fn message(&self) -> String {
        match self {
            Failure::Permanent(m) | Failure::Transient(m) => m.clone(),
            Failure::RateLimited(d) => format!("rate limited, retry in {} s", d.as_secs().max(1)),
            Failure::MessageGone => "the message no longer exists".into(),
        }
    }

    /// Whether the board should pause at once.
    pub fn is_permanent(&self) -> bool {
        matches!(self, Failure::Permanent(_))
    }
}

/// An answer to a request.
struct Answer {
    status: u16,
    body: Value,
}

/// The webhook client. One per process ([`client`]); tests build their own.
pub struct Client {
    /// Tests only: allow plain `http` and any host when parsing.
    relaxed: bool,
    /// How a Fluxer host name is turned into addresses.
    resolver: Resolver,
    /// Whether every resolved address has to be public (always, outside tests).
    public_only: bool,
    /// Webhook (API base and id) → when its rate limit lifts.
    buckets: Mutex<HashMap<String, Instant>>,
    /// API base → when it was last confirmed to be a Fluxer instance.
    confirmed: Mutex<HashMap<String, Instant>>,
    /// Shared client for Discord (fixed hosts), without redirects.
    discord: reqwest::Client,
}

/// Where a Fluxer host's addresses come from.
enum Resolver {
    /// The system resolver.
    System,
    /// Tests: a fixed answer, whatever the name.
    #[cfg(test)]
    Fixed(Vec<SocketAddr>),
    /// Tests: no pinning at all, connect to what the URL says.
    #[cfg(test)]
    Direct,
}

static CLIENT: LazyLock<Client> = LazyLock::new(|| Client::new(false, Resolver::System, true));

/// The process-wide client.
pub fn client() -> &'static Client {
    &CLIENT
}

fn base_builder() -> reqwest::ClientBuilder {
    crate::tls_roots::http_client()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(crate::product::USER_AGENT)
}

impl Client {
    fn new(relaxed: bool, resolver: Resolver, public_only: bool) -> Client {
        Client {
            relaxed,
            resolver,
            public_only,
            buckets: Mutex::default(),
            confirmed: Mutex::default(),
            discord: base_builder().build().expect("failed to build HTTP client"),
        }
    }

    /// A client that talks to loopback servers over plain `http`.
    #[cfg(test)]
    pub fn for_tests() -> Client {
        Client::new(true, Resolver::Direct, false)
    }

    /// A test client whose Fluxer host names all resolve to `addrs`, with
    /// or without the public-address rule: the real request path.
    #[cfg(test)]
    pub fn for_tests_resolving(addrs: Vec<SocketAddr>, public_only: bool) -> Client {
        Client::new(true, Resolver::Fixed(addrs), public_only)
    }

    /// Parses `url` the way this client accepts it.
    pub fn parse(&self, platform: Platform, url: &str) -> Result<Target, UrlError> {
        Target::parse_with(platform, url, self.relaxed)
    }

    /// The reqwest client for one request to `t`, after the checks that
    /// have to hold for every request: a Fluxer host is a Fluxer instance
    /// and resolves to public addresses only (which the connection is then
    /// pinned to).
    async fn client_for(&self, t: &Target) -> Result<reqwest::Client, Failure> {
        if t.platform == Platform::Discord {
            return Ok(self.discord.clone());
        }
        let addrs = match &self.resolver {
            Resolver::System => {
                let found: Vec<SocketAddr> = tokio::net::lookup_host((t.host.as_str(), t.port))
                    .await
                    .map_err(|_| Failure::Transient("could not resolve the host name".into()))?
                    .collect();
                check_public(found, self.public_only)?
            }
            #[cfg(test)]
            Resolver::Fixed(addrs) => check_public(addrs.clone(), self.public_only)?,
            #[cfg(test)]
            Resolver::Direct => Vec::new(),
        };
        let mut builder = base_builder().no_proxy();
        if !addrs.is_empty() {
            builder = builder.resolve_to_addrs(&t.host, &addrs);
        }
        let client = builder
            .build()
            .map_err(|_| Failure::Transient("could not build the HTTP client".into()))?;
        self.confirm_with(&client, t).await?;
        Ok(client)
    }

    /// Checks that `t`'s host is a Fluxer instance (cached for an hour).
    pub async fn confirm_instance(&self, t: &Target) -> Result<(), Failure> {
        if t.platform != Platform::Fluxer {
            return Ok(());
        }
        let client = self.client_for(t).await?;
        drop(client);
        Ok(())
    }

    async fn confirm_with(&self, client: &reqwest::Client, t: &Target) -> Result<(), Failure> {
        let key = t.api_base();
        {
            let confirmed = self
                .confirmed
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if confirmed
                .get(&key)
                .is_some_and(|at| at.elapsed() < DISCOVERY_TTL)
            {
                return Ok(());
            }
        }
        let not_fluxer = || Failure::Permanent("the host is not a Fluxer instance".to_string());
        let resp = client
            .get(format!("{}/.well-known/fluxer", t.origin))
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| {
                Failure::Transient(format!(
                    "could not reach the Fluxer instance ({})",
                    describe(&e)
                ))
            })?;
        if !resp.status().is_success() {
            return Err(not_fluxer());
        }
        let bytes = read_capped(resp, DISCOVERY_MAX_BYTES)
            .await
            .map_err(|_| not_fluxer())?;
        let doc: Value = serde_json::from_slice(&bytes).map_err(|_| not_fluxer())?;
        if !discovery_matches(&doc, t) {
            return Err(not_fluxer());
        }
        self.confirmed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, Instant::now());
        Ok(())
    }

    /// One request with the rate-limit rules: wait for a known limit (when it
    /// is short), honour a 429 once, remember the bucket's headers.
    async fn send(
        &self,
        t: &Target,
        method: reqwest::Method,
        url: String,
        body: Option<&Value>,
    ) -> Result<Answer, Failure> {
        let mut retried = false;
        loop {
            self.wait_for_bucket(&t.bucket_key()).await?;
            let client = self.client_for(t).await?;
            let mut req = client.request(method.clone(), &url);
            if let Some(body) = body {
                req = req.json(body);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| Failure::Transient(format!("request failed ({})", describe(&e))))?;
            let status = resp.status().as_u16();
            let headers = resp.headers().clone();
            self.note_headers(&t.bucket_key(), &headers);
            let bytes = read_capped(resp, 256 * 1024).await.unwrap_or_default();
            let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            if status == 429 {
                let wait = retry_after(&headers, &json);
                if !retried && wait <= MAX_INLINE_WAIT {
                    retried = true;
                    tokio::time::sleep(wait).await;
                    continue;
                }
                self.hold(&t.bucket_key(), wait);
                return Err(Failure::RateLimited(wait));
            }
            return Ok(Answer { status, body: json });
        }
    }

    async fn wait_for_bucket(&self, id: &str) -> Result<(), Failure> {
        let wait = {
            let buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
            buckets
                .get(id)
                .map(|until| until.saturating_duration_since(Instant::now()))
                .unwrap_or_default()
        };
        if wait.is_zero() {
            return Ok(());
        }
        if wait > MAX_INLINE_WAIT {
            return Err(Failure::RateLimited(wait));
        }
        tokio::time::sleep(wait).await;
        Ok(())
    }

    fn hold(&self, id: &str, wait: Duration) {
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        if buckets.len() > 1024 {
            let now = Instant::now();
            buckets.retain(|_, until| *until > now);
        }
        buckets.insert(id.to_string(), Instant::now() + wait.min(MAX_RATE_WAIT));
    }

    /// `X-RateLimit-Remaining: 0` means the next request has to wait
    /// `X-RateLimit-Reset-After` seconds (Discord and Fluxer send both).
    fn note_headers(&self, id: &str, headers: &reqwest::header::HeaderMap) {
        let number = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite())
        };
        if number("x-ratelimit-remaining").is_some_and(|r| r < 1.0)
            && let Some(after) = number("x-ratelimit-reset-after")
        {
            self.hold(id, secs(after, 0.0, 120.0));
        }
    }

    // -- Operations ---------------------------------------------------------

    /// Posts a message and returns its id.
    pub async fn create(&self, t: &Target, payload: &Value) -> Result<String, Failure> {
        let url = format!("{}?wait=true", t.execute_url());
        let answer = self
            .send(t, reqwest::Method::POST, url, Some(payload))
            .await?;
        match answer.status {
            200 | 201 => answer
                .body
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| is_digits(id, 30))
                .map(str::to_string)
                .ok_or_else(|| Failure::Transient("the answer had no valid message id".into())),
            _ => Err(failure_for(t, &answer)),
        }
    }

    /// Posts a message and does not ask for it back.
    pub async fn post(&self, t: &Target, payload: &Value) -> Result<(), Failure> {
        let answer = self
            .send(t, reqwest::Method::POST, t.execute_url(), Some(payload))
            .await?;
        match answer.status {
            200..=204 => Ok(()),
            _ => Err(failure_for(t, &answer)),
        }
    }

    /// Replaces a message's content in place. [`Failure::MessageGone`] when
    /// it was deleted.
    pub async fn edit(&self, t: &Target, message_id: &str, payload: &Value) -> Result<(), Failure> {
        let answer = self
            .send(
                t,
                reqwest::Method::PATCH,
                t.message_url(message_id),
                Some(payload),
            )
            .await?;
        match answer.status {
            200 | 204 => Ok(()),
            404 if is_unknown_message(&answer.body) => Err(Failure::MessageGone),
            _ => Err(failure_for(t, &answer)),
        }
    }

    /// Deletes a message; one that is gone already counts as deleted.
    pub async fn delete(&self, t: &Target, message_id: &str) -> Result<(), Failure> {
        let answer = self
            .send(t, reqwest::Method::DELETE, t.message_url(message_id), None)
            .await?;
        match answer.status {
            200 | 204 => Ok(()),
            404 if is_unknown_message(&answer.body) => Ok(()),
            _ => Err(failure_for(t, &answer)),
        }
    }
}

/// A 404 that names the message (Discord code 10008, Fluxer
/// `UNKNOWN_MESSAGE`), as opposed to a webhook that is gone (10015,
/// `UNKNOWN_WEBHOOK`).
fn is_unknown_message(body: &Value) -> bool {
    let code = body.get("code");
    let by_number = code.and_then(Value::as_i64) == Some(10008);
    let by_name = code
        .and_then(Value::as_str)
        .is_some_and(|c| c.eq_ignore_ascii_case("UNKNOWN_MESSAGE"));
    by_number || by_name
}

fn failure_for(t: &Target, answer: &Answer) -> Failure {
    let detail = answer
        .body
        .get("message")
        .and_then(Value::as_str)
        .map(|m| t.scrub(m))
        .map(|m| m.chars().take(160).collect::<String>())
        .unwrap_or_default();
    let name = t.platform.label();
    let text = if detail.is_empty() {
        format!("{name} answered {}", answer.status)
    } else {
        format!("{name} answered {}: {detail}", answer.status)
    };
    match answer.status {
        401 | 403 | 404 => Failure::Permanent(format!(
            "{text}. The webhook may have been deleted, or it has no access to the channel."
        )),
        400 => Failure::Permanent(text),
        s if s >= 500 => Failure::Transient(text),
        _ => Failure::Transient(text),
    }
}

fn describe(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timed out"
    } else if e.is_connect() {
        "could not connect"
    } else {
        "network error"
    }
}

async fn read_capped(resp: reqwest::Response, max: usize) -> Result<Vec<u8>, ()> {
    use futures_util::StreamExt as _;
    let mut stream = resp.bytes_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ())?;
        if out.len() + chunk.len() > max {
            return Err(());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Seconds a 429 asks us to wait (`retry_after` in the body, else the
/// `Retry-After` header), at least a short pause.
fn retry_after(headers: &reqwest::header::HeaderMap, body: &Value) -> Duration {
    let value = body
        .get("retry_after")
        .and_then(Value::as_f64)
        .or_else(|| {
            headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<f64>().ok())
        })
        .filter(|v| v.is_finite())
        .unwrap_or(1.0);
    secs(value, 0.1, 3600.0)
}

/// A duration of `value` seconds within `min` to `max`. Never panics: a
/// value that is not finite is `min`.
fn secs(value: f64, min: f64, max: f64) -> Duration {
    let v = if value.is_finite() {
        value.clamp(min, max)
    } else {
        min
    };
    Duration::try_from_secs_f64(v).unwrap_or(Duration::from_secs_f64(min))
}

/// The addresses a name resolved to, only if every one is public (when
/// `public_only`): a name that also points inside can't be used to reach
/// internal services.
fn check_public(addrs: Vec<SocketAddr>, public_only: bool) -> Result<Vec<SocketAddr>, Failure> {
    if addrs.is_empty() {
        return Err(Failure::Transient("the host name has no address".into()));
    }
    if public_only && addrs.iter().any(|a| !crate::kit::net::is_public(a.ip())) {
        return Err(Failure::Permanent(
            "the host does not resolve to a public address".into(),
        ));
    }
    Ok(addrs)
}

/// Whether a discovery document is a Fluxer instance's and lists `t`'s API
/// base as one of its endpoints. The document is `/.well-known/fluxer`; the
/// URL's prefix (without a trailing `/v1`) has to be its `api_public`,
/// `api_client` or `api` endpoint.
fn discovery_matches(doc: &Value, t: &Target) -> bool {
    if doc
        .get("api_code_version")
        .and_then(Value::as_i64)
        .is_none()
    {
        return false;
    }
    let Some(endpoints) = doc.get("endpoints").and_then(Value::as_object) else {
        return false;
    };
    let base = t.api_base();
    let base = base.strip_suffix("/v1").unwrap_or(&base);
    ["api_public", "api_client", "api"].iter().any(|k| {
        endpoints
            .get(*k)
            .and_then(Value::as_str)
            .is_some_and(|e| e.trim_end_matches('/').eq_ignore_ascii_case(base))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "123456789012345678";

    fn discord(url: &str) -> Result<Target, UrlError> {
        Target::parse(Platform::Discord, url)
    }

    #[test]
    fn discord_webhooks_on_discord_hosts_are_accepted() {
        for host in DISCORD_HOSTS {
            let t = discord(&format!("https://{host}/api/webhooks/{ID}/abc_DEF-123")).unwrap();
            assert_eq!(t.api_base(), format!("https://{host}/api"));
            assert_eq!(t.id, ID);
        }
        let t = discord(&format!("https://discord.com/api/v10/webhooks/{ID}/tok/")).unwrap();
        assert_eq!(t.prefix, "/api/v10");
        assert_eq!(
            t.execute_url(),
            format!("https://discord.com/api/v10/webhooks/{ID}/tok")
        );
    }

    #[test]
    fn other_hosts_schemes_and_shapes_are_refused_for_discord() {
        let bad_host = [
            format!("https://example.com/api/webhooks/{ID}/tok"),
            format!("https://discord.com.evil.test/api/webhooks/{ID}/tok"),
            format!("https://evil.test@discord.com/api/webhooks/{ID}/tok"),
            format!("https://127.0.0.1/api/webhooks/{ID}/tok"),
            format!("https://[::1]/api/webhooks/{ID}/tok"),
            format!("https://cdn.discordapp.com/api/webhooks/{ID}/tok"),
        ];
        for url in bad_host {
            assert!(discord(&url).is_err(), "{url}");
        }
        assert_eq!(
            discord(&format!("http://discord.com/api/webhooks/{ID}/tok")),
            Err(UrlError::Scheme)
        );
        assert_eq!(
            discord(&format!(
                "https://discord.com/api/webhooks/{ID}/tok?wait=true"
            )),
            Err(UrlError::Scheme)
        );
        assert_eq!(
            discord(&format!("https://discord.com/api/webhooks/{ID}/tok#x")),
            Err(UrlError::Scheme)
        );
        for path in [
            "/webhooks/1/tok",
            "/api/webhooks/abc/tok",
            "/api/webhooks/1",
            "/api/webhooks/1/tok/messages/2",
            "/api/webhooks/1/to%20k",
            "/api/hooks/1/tok",
            "/api/x/webhooks/1/tok",
        ] {
            assert_eq!(
                discord(&format!("https://discord.com{path}")),
                Err(UrlError::Path),
                "{path}"
            );
        }
        // Other ways to write a loopback address, a trailing dot, another port, other scripts.
        for host in [
            "2130706433",
            "0x7f.1",
            "0177.0.0.1",
            "[::ffff:127.0.0.1]",
            "[::ffff:7f00:1]",
            "localhost.",
            "discord.com.",
            "discord.com:8443",
            "discord.com:80",
            "d{456}scord.com",
            "xn--dscord-3ve.com",
            "discord{ff0e}com.evil.test",
        ] {
            assert!(
                discord(&format!("https://{host}/api/webhooks/{ID}/tok")).is_err(),
                "{host}"
            );
        }
        // An explicit default port is the same address.
        assert!(discord(&format!("https://discord.com:443/api/webhooks/{ID}/tok")).is_ok());
        assert_eq!(discord("not a url"), Err(UrlError::Invalid));
        assert_eq!(discord(""), Err(UrlError::Invalid));
    }

    #[test]
    fn fluxer_webhooks_take_any_named_host_with_an_optional_prefix() {
        let f = |url: &str| Target::parse(Platform::Fluxer, url);
        let t = f(&format!("https://chat.example.org/api/webhooks/{ID}/tok")).unwrap();
        assert_eq!(t.origin, "https://chat.example.org");
        assert_eq!(t.prefix, "/api");
        let t = f(&format!("https://api.fluxer.example/webhooks/{ID}/tok")).unwrap();
        assert_eq!(t.prefix, "");
        assert_eq!(
            f(&format!(
                "https://chat.example.org:8443/api/webhooks/{ID}/tok"
            ))
            .unwrap()
            .origin,
            "https://chat.example.org:8443"
        );
        // Never an address, `localhost` or a name without a dot.
        for host in [
            "127.0.0.1",
            "10.0.0.5",
            "[::1]",
            "localhost",
            "localhost.",
            "app.localhost",
            "intranet",
            "2130706433",
            "0x7f.1",
            "[::ffff:127.0.0.1]",
            "chat.example.org.",
        ] {
            assert_eq!(
                f(&format!("https://{host}/api/webhooks/{ID}/tok")),
                Err(UrlError::Host),
                "{host}"
            );
        }
        assert_eq!(
            f(&format!("http://chat.example.org/api/webhooks/{ID}/tok")),
            Err(UrlError::Scheme)
        );
        assert_eq!(
            f(&format!(
                "https://chat.example.org/api/webhooks/{ID}/tok?x=1"
            )),
            Err(UrlError::Scheme)
        );
    }

    #[test]
    fn discovery_must_name_the_urls_api_base() {
        let t = Target::parse(
            Platform::Fluxer,
            &format!("https://chat.example.org/api/webhooks/{ID}/tok"),
        )
        .unwrap();
        let doc =
            |endpoints: Value| serde_json::json!({"api_code_version": 5, "endpoints": endpoints});
        assert!(discovery_matches(
            &doc(serde_json::json!({"api_public": "https://chat.example.org/api"})),
            &t
        ));
        assert!(discovery_matches(
            &doc(serde_json::json!({"api": "https://chat.example.org/api/"})),
            &t
        ));
        // Another host's endpoint, a missing version and a plain JSON file are not enough.
        assert!(!discovery_matches(
            &doc(serde_json::json!({"api_public": "https://evil.example/api"})),
            &t
        ));
        assert!(!discovery_matches(
            &serde_json::json!({"endpoints": {"api_public": "https://chat.example.org/api"}}),
            &t
        ));
        assert!(!discovery_matches(&serde_json::json!({"hello": 1}), &t));
        // `/v1` on the URL is the same API.
        let v1 = Target::parse(
            Platform::Fluxer,
            &format!("https://chat.example.org/api/v1/webhooks/{ID}/tok"),
        )
        .unwrap();
        assert!(discovery_matches(
            &doc(serde_json::json!({"api_public": "https://chat.example.org/api"})),
            &v1
        ));
    }

    #[test]
    fn the_token_never_shows_in_debug_output_or_errors() {
        let t = discord(&format!("https://discord.com/api/webhooks/{ID}/SECRETtok")).unwrap();
        assert!(!format!("{t:?}").contains("SECRETtok"));
        let f = failure_for(
            &t,
            &Answer {
                status: 401,
                body: serde_json::json!({"message": "bad token SECRETtok"}),
            },
        );
        assert!(!f.message().contains("SECRETtok"), "{}", f.message());
        assert!(f.is_permanent());
    }

    #[test]
    fn retry_after_reads_the_body_then_the_header() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "7".parse().unwrap());
        assert_eq!(
            retry_after(&headers, &serde_json::json!({"retry_after": 2.5})),
            Duration::from_secs_f64(2.5)
        );
        assert_eq!(retry_after(&headers, &Value::Null), Duration::from_secs(7));
        assert_eq!(
            retry_after(&reqwest::header::HeaderMap::new(), &Value::Null),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn unknown_message_is_told_from_unknown_webhook() {
        assert!(is_unknown_message(&serde_json::json!({"code": 10008})));
        assert!(is_unknown_message(
            &serde_json::json!({"code": "UNKNOWN_MESSAGE"})
        ));
        assert!(!is_unknown_message(&serde_json::json!({"code": 10015})));
        assert!(!is_unknown_message(
            &serde_json::json!({"code": "UNKNOWN_WEBHOOK"})
        ));
        assert!(!is_unknown_message(&Value::Null));
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;

    #[tokio::test]
    async fn names_that_resolve_to_loopback_or_private_addresses_are_refused() {
        let addr = |s: &str| -> SocketAddr { s.parse().unwrap() };
        for bad in [
            vec![addr("127.0.0.1:443")],
            vec![addr("10.0.0.5:443")],
            vec![addr("169.254.169.254:443")],
            vec![addr("[::1]:443")],
            vec![addr("[::ffff:127.0.0.1]:443")],
            // One public answer does not excuse a private one.
            vec![addr("93.184.216.34:443"), addr("192.168.1.1:443")],
        ] {
            assert!(
                matches!(check_public(bad.clone(), true), Err(Failure::Permanent(_))),
                "{bad:?}"
            );
        }
        assert!(check_public(vec![addr("93.184.216.34:443")], true).is_ok());
        assert!(matches!(
            check_public(vec![], true),
            Err(Failure::Transient(_))
        ));
    }

    #[test]
    fn rate_limit_numbers_that_are_not_finite_never_panic() {
        for bad in ["nan", "NaN", "inf", "-inf", "infinity", "1e999"] {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("retry-after", bad.parse().unwrap());
            let wait = retry_after(&headers, &Value::Null);
            assert!(
                wait >= Duration::from_millis(100) && wait <= Duration::from_secs(3600),
                "{bad}: {wait:?}"
            );
            // A body number that is not finite can't be sent as JSON, but a string is ignored.
            assert!(
                retry_after(
                    &reqwest::header::HeaderMap::new(),
                    &serde_json::json!({"retry_after": bad})
                ) >= Duration::from_millis(100)
            );

            let client = Client::for_tests();
            let mut h = reqwest::header::HeaderMap::new();
            h.insert("x-ratelimit-remaining", "0".parse().unwrap());
            h.insert("x-ratelimit-reset-after", bad.parse().unwrap());
            client.note_headers("k", &h);
            let mut h = reqwest::header::HeaderMap::new();
            h.insert("x-ratelimit-remaining", bad.parse().unwrap());
            h.insert("x-ratelimit-reset-after", "1".parse().unwrap());
            client.note_headers("k", &h);
        }
        assert_eq!(secs(f64::NAN, 0.1, 5.0), Duration::from_millis(100));
        assert_eq!(secs(f64::INFINITY, 0.1, 5.0), Duration::from_millis(100));
        assert_eq!(secs(1e300, 0.1, 5.0), Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_rate_limit_header_holds_the_next_request_for_that_webhook() {
        let client = Client::for_tests();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        headers.insert("x-ratelimit-reset-after", "0.3".parse().unwrap());
        client.note_headers("111", &headers);
        let started = Instant::now();
        client.wait_for_bucket("111").await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(250));
        // Another webhook is not held, and requests left in the bucket don't wait.
        let started = Instant::now();
        client.wait_for_bucket("222").await.unwrap();
        headers.insert("x-ratelimit-remaining", "3".parse().unwrap());
        client.note_headers("333", &headers);
        client.wait_for_bucket("333").await.unwrap();
        assert!(started.elapsed() < Duration::from_millis(100));
        // A wait beyond the inline limit is handed back to the board.
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        headers.insert("x-ratelimit-reset-after", "60".parse().unwrap());
        client.note_headers("444", &headers);
        assert!(matches!(
            client.wait_for_bucket("444").await,
            Err(Failure::RateLimited(_))
        ));
    }
}
