//! HTTP monitor authentication beyond basic auth.
//!
//! * **OAuth2 client credentials** (RFC 6749 §4.4): the checker POSTs to the
//!   token endpoint, caches the access token per monitor until shortly before
//!   it expires ([`TokenCache`]), and sends it as `Authorization: Bearer …`.
//!   A rejected request with a cached token drops the cache and retries once
//!   with a fresh one (revoked / rotated tokens).
//! * **mTLS**: PEM client certificate + private key (reqwest's rustls
//!   [`reqwest::Identity`]) and an optional extra CA certificate to trust.
//!
//! Secrets (client secret, private key, tokens) never appear in the messages
//! built here; [`redact`] is the last line of defence for the ones that
//! travel through error text.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

use super::util;
use crate::db::models::Monitor;

/// Values of `monitors.auth_method`.
pub const AUTH_METHODS: &[&str] = &["none", "basic", "oauth2", "mtls"];
/// Values of `monitors.oauth_client_auth`.
pub const OAUTH_CLIENT_AUTHS: &[&str] = &["client_secret_basic", "client_secret_post"];

/// Token responses are read up to this many bytes.
const MAX_TOKEN_BODY: usize = 64 * 1024;
/// How long a token without `expires_in` is trusted.
const DEFAULT_TOKEN_TTL_SECS: u64 = 300;
/// Most tokens kept at once (one per OAuth2 monitor).
const MAX_TOKENS: usize = 4096;

/// The method meant by input that doesn't name one: clients, forms and
/// config files from before `auth_method` existed only knew basic auth, so a
/// user name means basic. Each input boundary (form posts, the v1 API, config
/// documents, agents fed by an older server) applies this once when the
/// method is missing or empty; an explicit `none` is always respected. Stored
/// monitors carry their method (migration 0096 converted the old rows) and
/// the checker uses `monitor.auth_method` as is.
pub fn implied_method(basic_auth_user: Option<&str>) -> &'static str {
    if basic_auth_user.is_some_and(|u| !u.trim().is_empty()) { "basic" } else { "none" }
}

/// Replace every given secret in `message` (empty ones are ignored).
pub fn redact(message: &str, secrets: &[&str]) -> String {
    let mut out = message.to_string();
    for s in secrets.iter().filter(|s| !s.is_empty()) {
        out = out.replace(s, "[redacted]");
    }
    out
}

// ---------------------------------------------------------------------------
// mTLS
// ---------------------------------------------------------------------------

/// Why a client certificate / key / CA was refused. The monitor form maps
/// each variant to a message on the field that caused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MtlsError {
    MissingCert,
    CertNotPem,
    MissingKey,
    KeyNotPem,
    KeyEncrypted,
    CaNotPem,
    /// rustls refused the certificate/key pair (e.g. the key doesn't match).
    Rejected(String),
    /// The CA certificate parsed as PEM but could not be used.
    CaRejected(String),
}

impl fmt::Display for MtlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MtlsError::MissingCert => f.write_str("No client certificate configured"),
            MtlsError::CertNotPem => f.write_str("The client certificate is not PEM (expected -----BEGIN CERTIFICATE-----)"),
            MtlsError::MissingKey => f.write_str("No client private key configured"),
            MtlsError::KeyNotPem => f.write_str("The client private key is not an unencrypted PEM key"),
            MtlsError::KeyEncrypted => f.write_str("The client private key is passphrase-protected, which is not supported"),
            MtlsError::CaNotPem => f.write_str("The CA certificate is not PEM (expected -----BEGIN CERTIFICATE-----)"),
            MtlsError::Rejected(why) => write!(f, "The client certificate and key can't be used together: {why}"),
            MtlsError::CaRejected(why) => write!(f, "The CA certificate can't be used: {why}"),
        }
    }
}

const PEM_CERT: &str = "-----BEGIN CERTIFICATE-----";

/// What is wrong with the shape of the three PEM texts, one entry per field.
fn pem_problems(cert: &str, key: &str, ca: Option<&str>) -> Vec<MtlsError> {
    let mut out = Vec::new();
    if cert.trim().is_empty() {
        out.push(MtlsError::MissingCert);
    } else if !cert.contains(PEM_CERT) {
        out.push(MtlsError::CertNotPem);
    }
    let is_key = ["-----BEGIN PRIVATE KEY-----", "-----BEGIN RSA PRIVATE KEY-----", "-----BEGIN EC PRIVATE KEY-----"]
        .iter()
        .any(|marker| key.contains(marker));
    if key.trim().is_empty() {
        out.push(MtlsError::MissingKey);
    } else if key.contains("ENCRYPTED") {
        out.push(MtlsError::KeyEncrypted);
    } else if !is_key {
        out.push(MtlsError::KeyNotPem);
    }
    if ca.is_some_and(|ca| !ca.trim().is_empty() && !ca.contains(PEM_CERT)) {
        out.push(MtlsError::CaNotPem);
    }
    out
}

fn check_pem(cert: &str, key: &str, ca: Option<&str>) -> Result<(), MtlsError> {
    pem_problems(cert, key, ca).into_iter().next().map_or(Ok(()), Err)
}

/// Adds the client identity (and the extra CA, when given) to `builder`.
pub fn apply_mtls(
    builder: reqwest::ClientBuilder,
    cert: &str,
    key: &str,
    ca: Option<&str>,
) -> Result<reqwest::ClientBuilder, MtlsError> {
    let ca = ca.map(str::trim).filter(|c| !c.is_empty());
    check_pem(cert, key, ca)?;
    let pem = format!("{}\n{}\n", cert.trim(), key.trim());
    let identity = reqwest::Identity::from_pem(pem.as_bytes()).map_err(|e| MtlsError::Rejected(util::root_cause(&e)))?;
    let mut builder = builder.identity(identity);
    if let Some(ca) = ca {
        let roots = reqwest::Certificate::from_pem_bundle(ca.as_bytes()).map_err(|_| MtlsError::CaNotPem)?;
        if roots.is_empty() {
            return Err(MtlsError::CaNotPem);
        }
        builder = builder.tls_certs_merge(roots);
    }
    Ok(builder)
}

/// Everything wrong with the material, for the form: malformed fields
/// first (all of them), otherwise whatever rustls refuses.
pub fn mtls_problems(cert: &str, key: &str, ca: Option<&str>) -> Vec<MtlsError> {
    let shape = pem_problems(cert, key, ca.map(str::trim));
    if !shape.is_empty() {
        return shape;
    }
    validate_mtls(cert, key, ca).err().into_iter().collect()
}

/// Builds a throwaway client from the material, so a bad pair is reported in
/// the form instead of at the first check.
pub fn validate_mtls(cert: &str, key: &str, ca: Option<&str>) -> Result<(), MtlsError> {
    apply_mtls(crate::tls_roots::http_client(), cert, key, None)?
        .build()
        .map_err(|e| MtlsError::Rejected(util::root_cause(&e)))?;
    if let Some(ca) = ca.map(str::trim).filter(|c| !c.is_empty()) {
        apply_mtls(crate::tls_roots::http_client(), cert, key, Some(ca))?
            .build()
            .map_err(|e| MtlsError::CaRejected(util::root_cause(&e)))?;
    }
    Ok(())
}

/// Stable, non-reversible id of the TLS material, for the client cache key.
pub fn mtls_fingerprint(monitor: &Monitor) -> Option<String> {
    if monitor.auth_method != "mtls" {
        return None;
    }
    let part = |s: &Option<String>| s.as_deref().unwrap_or_default().trim().to_string();
    Some(digest(&[&part(&monitor.tls_client_cert), &part(&monitor.tls_client_key), &part(&monitor.tls_ca_cert)]))
}

fn digest(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_be_bytes());
        h.update(p.as_bytes());
    }
    hex::encode(h.finalize())
}

// ---------------------------------------------------------------------------
// OAuth2 client credentials
// ---------------------------------------------------------------------------

/// The OAuth2 settings of a monitor, checked.
#[derive(Clone)]
pub struct OAuthConfig {
    pub token_url: Url,
    pub client_id: String,
    pub client_secret: String,
    pub scope: Option<String>,
    pub audience: Option<String>,
    /// `true` = `client_secret_post` (credentials in the form body).
    pub secret_in_body: bool,
}

// Hand-written so a stray `{:?}` can't print the secret.
impl fmt::Debug for OAuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthConfig")
            .field("token_url", &self.token_url.as_str())
            .field("client_id", &self.client_id)
            .field("client_secret", &"[redacted]")
            .finish_non_exhaustive()
    }
}

/// Parses a token endpoint URL (http or https with a host).
pub fn parse_token_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw.trim()).map_err(|e| format!("The OAuth2 token URL is not a valid URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
        return Err("The OAuth2 token URL must be an http or https URL".into());
    }
    Ok(url)
}

impl OAuthConfig {
    pub fn from_monitor(m: &Monitor) -> Result<Self, String> {
        let text = |v: &Option<String>| v.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        let token_url = text(&m.oauth_token_url).ok_or("No OAuth2 token URL configured")?;
        Ok(Self {
            token_url: parse_token_url(&token_url)?,
            client_id: text(&m.oauth_client_id).ok_or("No OAuth2 client ID configured")?,
            // Not trimmed: a secret may legitimately end in a space.
            client_secret: m
                .oauth_client_secret
                .clone()
                .filter(|s| !s.is_empty())
                .ok_or("No OAuth2 client secret configured")?,
            scope: text(&m.oauth_scope),
            audience: text(&m.oauth_audience),
            secret_in_body: m.oauth_client_auth == "client_secret_post",
        })
    }

    /// Changes when any setting that affects the issued token changes, so
    /// editing a monitor never reuses a token from the old settings.
    fn fingerprint(&self) -> String {
        digest(&[
            self.token_url.as_str(),
            &self.client_id,
            &self.client_secret,
            self.scope.as_deref().unwrap_or_default(),
            self.audience.as_deref().unwrap_or_default(),
            if self.secret_in_body { "post" } else { "basic" },
        ])
    }

    fn secrets(&self) -> Vec<&str> {
        vec![self.client_secret.as_str()]
    }
}

/// `Authorization: Basic …` value for the token endpoint (RFC 6749 §2.3.1:
/// id and secret are form-urlencoded before the base64 step).
pub fn client_secret_basic(client_id: &str, client_secret: &str) -> String {
    let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    let raw = format!("{}:{}", enc(client_id), enc(client_secret));
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw))
}

/// How long to trust a token that expires in `expires_in` seconds: until
/// shortly before it runs out (10 %, between 1 and 60 seconds early).
pub fn refresh_after(expires_in: Option<u64>) -> Duration {
    match expires_in {
        None => Duration::from_secs(DEFAULT_TOKEN_TTL_SECS),
        Some(ttl) => Duration::from_secs(ttl.saturating_sub((ttl / 10).clamp(1, 60))),
    }
}

struct CachedToken {
    fingerprint: String,
    token: String,
    refresh_at: Instant,
}

/// Access tokens by monitor id. Clock is a parameter so expiry is testable.
/// Bounded: tokens due for a refresh are dropped on every `put` (they are
/// never served again), at most [`MAX_TOKENS`] are kept, and a monitor's
/// token is forgotten when the monitor is edited or deleted
/// ([`forget_token`], called by `Scheduler::reload`).
#[derive(Default)]
pub struct TokenCache {
    entries: Mutex<HashMap<i64, CachedToken>>,
}

impl TokenCache {
    /// The cached token, while it belongs to the same settings and is not
    /// due for a refresh.
    pub fn get(&self, monitor_id: i64, fingerprint: &str, now: Instant) -> Option<String> {
        let map = self.entries.lock().ok()?;
        let e = map.get(&monitor_id)?;
        (e.fingerprint == fingerprint && now < e.refresh_at).then(|| e.token.clone())
    }

    pub fn put(&self, monitor_id: i64, fingerprint: &str, token: &str, expires_in: Option<u64>, now: Instant) {
        if let Ok(mut map) = self.entries.lock() {
            map.retain(|_, e| now < e.refresh_at);
            if map.len() >= MAX_TOKENS
                && !map.contains_key(&monitor_id)
                && let Some(first_due) = map.iter().min_by_key(|(_, e)| e.refresh_at).map(|(id, _)| *id)
            {
                map.remove(&first_due);
            }
            map.insert(
                monitor_id,
                CachedToken {
                    fingerprint: fingerprint.to_string(),
                    token: token.to_string(),
                    refresh_at: now + refresh_after(expires_in),
                },
            );
        }
    }

    pub fn invalidate(&self, monitor_id: i64) {
        if let Ok(mut map) = self.entries.lock() {
            map.remove(&monitor_id);
        }
    }
}

pub(crate) fn cache() -> &'static TokenCache {
    static CACHE: OnceLock<TokenCache> = OnceLock::new();
    CACHE.get_or_init(TokenCache::default)
}

/// Drops the cached token of a monitor: after the target rejected it, and
/// whenever the monitor is edited, paused or deleted.
pub fn forget_token(monitor_id: i64) {
    cache().invalidate(monitor_id);
}

/// A bearer header for the monitor. The flag says whether the token came
/// from the cache. `Err` is a complete Down message.
pub async fn bearer(monitor: &Monitor, client: &reqwest::Client, timeout: Duration) -> Result<(HeaderValue, bool), String> {
    let cfg = OAuthConfig::from_monitor(monitor)?;
    let fingerprint = cfg.fingerprint();
    if let Some(token) = cache().get(monitor.id, &fingerprint, Instant::now()) {
        return Ok((bearer_value(&token)?, true));
    }
    let (token, expires_in) = fetch_token(client, &cfg, timeout).await?;
    cache().put(monitor.id, &fingerprint, &token, expires_in, Instant::now());
    Ok((bearer_value(&token)?, false))
}

fn bearer_value(token: &str) -> Result<HeaderValue, String> {
    let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| "OAuth2 token endpoint returned an access token that can't be sent in a header".to_string())?;
    value.set_sensitive(true);
    Ok(value)
}

/// Requests a new access token. Messages are final Down texts and never
/// contain the client secret.
async fn fetch_token(client: &reqwest::Client, cfg: &OAuthConfig, timeout: Duration) -> Result<(String, Option<u64>), String> {
    let mut form: Vec<(&str, &str)> = vec![("grant_type", "client_credentials")];
    if let Some(scope) = &cfg.scope {
        form.push(("scope", scope));
    }
    if let Some(audience) = &cfg.audience {
        form.push(("audience", audience));
    }
    let mut req = client
        .post(cfg.token_url.clone())
        .timeout(timeout)
        .header(ACCEPT, "application/json");
    if cfg.secret_in_body {
        form.push(("client_id", &cfg.client_id));
        form.push(("client_secret", &cfg.client_secret));
    } else {
        let mut value = HeaderValue::from_str(&client_secret_basic(&cfg.client_id, &cfg.client_secret))
            .map_err(|_| "OAuth2 client credentials can't be sent in a header".to_string())?;
        value.set_sensitive(true);
        req = req.header(AUTHORIZATION, value);
    }
    let fail = |why: String| redact(&format!("OAuth2 token request failed: {why}"), &cfg.secrets());

    let resp = req.form(&form).send().await.map_err(|e| {
        let why = if e.is_timeout() {
            format!("timed out after {}", util::format_secs(timeout))
        } else {
            util::root_cause(&e)
        };
        // reqwest messages can embed the URL, which may carry a query token.
        fail(why.replace(cfg.token_url.as_str(), "the token URL"))
    })?;
    let status = resp.status();
    let body = read_capped(resp).await.map_err(|e| fail(format!("reading the response failed: {}", util::root_cause(&e))))?;
    let json: Option<Value> = serde_json::from_str(&body).ok();

    if !status.is_success() {
        let field = |name: &str| {
            json.as_ref()
                .and_then(|j| j.get(name))
                .and_then(Value::as_str)
                .map(|s| util::truncate(&s.chars().filter(|c| !c.is_control()).collect::<String>(), 120))
        };
        let head = match status.canonical_reason() {
            Some(reason) => format!("HTTP {} {reason}", status.as_u16()),
            None => format!("HTTP {}", status.as_u16()),
        };
        let detail = match (field("error"), field("error_description")) {
            (Some(e), Some(d)) => format!(" ({e}: {d})"),
            (Some(e), None) => format!(" ({e})"),
            (None, Some(d)) => format!(" ({d})"),
            (None, None) => String::new(),
        };
        return Err(fail(format!("{head}{detail}")));
    }

    let Some(json) = json else {
        return Err(fail("the response is not JSON".into()));
    };
    let token = json.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty());
    let Some(token) = token else {
        return Err(fail("the response has no access_token".into()));
    };
    if let Some(kind) = json.get("token_type").and_then(Value::as_str)
        && !kind.eq_ignore_ascii_case("bearer")
    {
        return Err(fail(format!("token type “{}” is not supported (Bearer only)", util::truncate(kind, 30))));
    }
    let expires_in = json.get("expires_in").and_then(|v| match v {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    });
    Ok((token.to_string(), expires_in))
}

async fn read_capped(mut resp: reqwest::Response) -> Result<String, reqwest::Error> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        let room = MAX_TOKEN_BODY.saturating_sub(buf.len());
        buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if buf.len() >= MAX_TOKEN_BODY {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod integration;
#[cfg(test)]
pub(crate) mod test_pki;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::MonitorType;
    use crate::monitor::checkers::test_support::monitor;

    fn oauth_monitor() -> Monitor {
        let mut m = monitor(MonitorType::Http, "https://api.example.com/");
        m.auth_method = "oauth2".into();
        m.oauth_token_url = Some("https://idp.example.com/token".into());
        m.oauth_client_id = Some("vole".into());
        m.oauth_client_secret = Some("s3cr3t".into());
        m
    }

    #[test]
    fn a_missing_method_with_a_user_name_means_basic() {
        assert_eq!(implied_method(None), "none");
        assert_eq!(implied_method(Some(" ")), "none");
        assert_eq!(implied_method(Some("bot")), "basic");
    }

    #[test]
    fn basic_header_form_encodes_before_base64() {
        // RFC 6749 §2.3.1 example shape: "id:secret" with reserved characters.
        let header = client_secret_basic("my id", "a+b/c:d");
        let b64 = header.strip_prefix("Basic ").unwrap();
        let raw = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        assert_eq!(String::from_utf8(raw).unwrap(), "my+id:a%2Bb%2Fc%3Ad");
    }

    #[test]
    fn config_requires_url_id_and_secret() {
        let mut m = oauth_monitor();
        assert!(OAuthConfig::from_monitor(&m).is_ok());
        m.oauth_client_secret = None;
        assert!(OAuthConfig::from_monitor(&m).unwrap_err().contains("client secret"));
        m.oauth_client_secret = Some("x".into());
        m.oauth_token_url = Some("ftp://idp".into());
        assert!(OAuthConfig::from_monitor(&m).unwrap_err().contains("http or https"));
        m.oauth_token_url = None;
        assert!(OAuthConfig::from_monitor(&m).unwrap_err().contains("token URL"));
    }

    #[test]
    fn debug_output_never_shows_the_secret() {
        let cfg = OAuthConfig::from_monitor(&oauth_monitor()).unwrap();
        assert!(!format!("{cfg:?}").contains("s3cr3t"));
    }

    #[test]
    fn redact_removes_secrets() {
        assert_eq!(redact("bad s3cr3t here", &["s3cr3t"]), "bad [redacted] here");
        assert_eq!(redact("nothing", &[""]), "nothing");
    }

    #[test]
    fn refresh_happens_shortly_before_expiry() {
        assert_eq!(refresh_after(Some(3600)), Duration::from_secs(3540));
        assert_eq!(refresh_after(Some(300)), Duration::from_secs(270));
        assert_eq!(refresh_after(Some(5)), Duration::from_secs(4));
        assert_eq!(refresh_after(Some(0)), Duration::ZERO);
        assert_eq!(refresh_after(None), Duration::from_secs(DEFAULT_TOKEN_TTL_SECS));
    }

    #[test]
    fn cache_serves_until_refresh_time_and_per_monitor() {
        let cache = TokenCache::default();
        let t0 = Instant::now();
        cache.put(1, "fp", "tok", Some(100), t0);
        assert_eq!(cache.get(1, "fp", t0 + Duration::from_secs(80)).as_deref(), Some("tok"));
        // 100 s lifetime refreshes after 90 s.
        assert_eq!(cache.get(1, "fp", t0 + Duration::from_secs(90)), None);
        assert_eq!(cache.get(2, "fp", t0), None);
        // Other settings never get the old token.
        assert_eq!(cache.get(1, "other", t0), None);
        cache.invalidate(1);
        assert_eq!(cache.get(1, "fp", t0), None);
        // An already-expired token is never served.
        cache.put(1, "fp", "tok", Some(0), t0);
        assert_eq!(cache.get(1, "fp", t0), None);
    }

    #[test]
    fn cache_drops_stale_tokens_and_stays_bounded() {
        let cache = TokenCache::default();
        let t0 = Instant::now();
        let len = || cache.entries.lock().unwrap().len();
        cache.put(1, "fp", "old", Some(10), t0);
        cache.put(2, "fp", "tok", Some(1000), t0);
        // Monitor 1's token is due by now: the next put drops it.
        cache.put(3, "fp", "tok", Some(1000), t0 + Duration::from_secs(60));
        assert_eq!(len(), 2);
        for id in 10..10 + MAX_TOKENS as i64 {
            cache.put(id, "fp", "tok", Some(1000 + id as u64), t0);
        }
        assert_eq!(len(), MAX_TOKENS);
        // The token due first made room; the newest is kept.
        assert_eq!(cache.get(2, "fp", t0), None);
        assert!(cache.get(9 + MAX_TOKENS as i64, "fp", t0).is_some());
    }

    #[test]
    fn fingerprint_follows_every_token_setting() {
        let base = OAuthConfig::from_monitor(&oauth_monitor()).unwrap();
        let fp = base.fingerprint();
        for edit in [
            |c: &mut OAuthConfig| c.client_secret = "other".into(),
            |c: &mut OAuthConfig| c.client_id = "other".into(),
            |c: &mut OAuthConfig| c.scope = Some("read".into()),
            |c: &mut OAuthConfig| c.audience = Some("api".into()),
            |c: &mut OAuthConfig| c.secret_in_body = true,
            |c: &mut OAuthConfig| c.token_url = Url::parse("https://idp.example.com/other").unwrap(),
        ] {
            let mut c = base.clone();
            edit(&mut c);
            assert_ne!(c.fingerprint(), fp);
        }
    }

    #[test]
    fn mtls_pem_shape_is_checked_with_clear_errors() {
        const CERT: &str = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----";
        const KEY: &str = "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----";
        assert_eq!(check_pem("", KEY, None), Err(MtlsError::MissingCert));
        assert_eq!(check_pem("hello", KEY, None), Err(MtlsError::CertNotPem));
        assert_eq!(check_pem(CERT, " ", None), Err(MtlsError::MissingKey));
        assert_eq!(check_pem(CERT, CERT, None), Err(MtlsError::KeyNotPem));
        assert_eq!(check_pem(CERT, "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAA", None), Err(MtlsError::KeyEncrypted));
        assert_eq!(check_pem(CERT, KEY, Some("nope")), Err(MtlsError::CaNotPem));
        assert_eq!(check_pem(CERT, KEY, Some("")), Ok(()));
        assert_eq!(check_pem(CERT, KEY, Some(CERT)), Ok(()));
    }

    #[test]
    fn mtls_fingerprint_only_for_mtls_monitors() {
        let mut m = monitor(MonitorType::Http, "https://x.io");
        assert_eq!(mtls_fingerprint(&m), None);
        m.auth_method = "mtls".into();
        m.tls_client_cert = Some("c".into());
        let a = mtls_fingerprint(&m).unwrap();
        m.tls_client_key = Some("k".into());
        assert_ne!(mtls_fingerprint(&m).unwrap(), a);
        // Whitespace around PEM text doesn't create a new client.
        m.tls_client_key = Some("k\n".into());
        assert_eq!(mtls_fingerprint(&m), mtls_fingerprint(&{
            let mut n = m.clone();
            n.tls_client_key = Some("k".into());
            n
        }));
    }
}
