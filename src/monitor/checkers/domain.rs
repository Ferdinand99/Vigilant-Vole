//! Domain registration expiry checker (RDAP).
//!
//! - `target`: a domain, hostname or URL; reduced to the registrable domain
//!   (`www.shop.example.co.uk` → `example.co.uk`). Multi-label public
//!   suffixes come from a small built-in list ([`MULTI_LABEL_SUFFIXES`]), not
//!   the full Public Suffix List.
//! - The RDAP server is picked from the IANA bootstrap registry
//!   (`https://data.iana.org/rdap/dns.json`, cached 24 h) by TLD. TLDs
//!   without RDAP (several ccTLDs, e.g. .de) are reported as Down with
//!   "No RDAP server for .xx".
//! - Per-domain results are cached 12 h (permanent failures 1 h), so the
//!   check interval does not hammer registries.
//!
//! Up with [`ExpiryInfo`] (`kind: Domain`, subject = domain, issuer =
//! registrar) until the expiration date; Down once expired. The engine
//! handles "expires within N days" notifications.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::util;
use crate::db::models::{CheckOutcome, ExpiryInfo, ExpiryKind, HeartbeatStatus, Monitor};

pub const BOOTSTRAP_URL: &str = "https://data.iana.org/rdap/dns.json";
const BOOTSTRAP_TTL: Duration = Duration::from_secs(24 * 3600);
const RESULT_TTL: Duration = Duration::from_secs(12 * 3600);
const FAILURE_TTL: Duration = Duration::from_secs(3600);

/// Public suffixes with more than one label that we know about.
pub const MULTI_LABEL_SUFFIXES: &[&str] = &[
    "co.uk", "org.uk", "me.uk", "ltd.uk", "plc.uk", "net.uk", "ac.uk", "gov.uk", "sch.uk",
    "com.au", "net.au", "org.au", "edu.au", "gov.au", "id.au", "co.nz", "net.nz", "org.nz",
    "govt.nz", "ac.nz", "co.jp", "ne.jp", "or.jp", "ac.jp", "go.jp", "co.kr", "or.kr", "co.za",
    "org.za", "web.za", "com.br", "net.br", "org.br", "com.cn", "net.cn", "org.cn", "com.hk",
    "com.tw", "com.sg", "com.my", "co.in", "net.in", "org.in", "co.id", "co.il", "co.th", "com.mx",
    "com.ar", "com.co", "com.tr", "com.ua", "com.pl", "com.es", "co.at", "or.at", "com.ph",
    "com.vn", "com.pk", "com.ng", "com.eg", "com.sa", "co.ke", "com.pe", "com.ve", "com.ec",
    "com.uy",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainRecord {
    pub expires_unix: i64,
    pub registrar: Option<String>,
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Reduce a URL / hostname to its registrable domain (punycode, lowercase).
pub fn registrable_domain(target: &str) -> Result<String, String> {
    let mut t = target.trim();
    if let Some(idx) = t.find("://") {
        t = &t[idx + 3..];
    }
    let t = t.split(['/', '?', '#']).next().unwrap_or("");
    let t = t.rsplit_once('@').map_or(t, |(_, host)| host);
    if t.is_empty() {
        return Err("No domain configured".into());
    }
    // Let the URL parser validate and IDNA-encode the host (drops the port too).
    let url = url::Url::parse(&format!("http://{t}"))
        .map_err(|_| format!("\"{t}\" is not a valid domain"))?;
    let host = match url.host() {
        Some(url::Host::Domain(d)) => d.trim_end_matches('.').to_ascii_lowercase(),
        Some(_) => return Err("Domain expiry needs a domain name, not an IP address".into()),
        None => return Err(format!("\"{t}\" is not a valid domain")),
    };
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    if labels.len() < 2 {
        return Err(format!(
            "\"{host}\" is not a registrable domain (expected e.g. example.com)"
        ));
    }
    let last_two = labels[labels.len() - 2..].join(".");
    let take = if MULTI_LABEL_SUFFIXES.contains(&last_two.as_str()) {
        3
    } else {
        2
    };
    if labels.len() < take {
        return Err(format!(
            "\"{host}\" is a public suffix, not a registrable domain"
        ));
    }
    Ok(labels[labels.len() - take..].join("."))
}

/// Map TLD → RDAP base URL (with trailing slash, https preferred) from the
/// IANA bootstrap document.
pub fn parse_bootstrap(doc: &Value) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(services) = doc.get("services").and_then(Value::as_array) else {
        return map;
    };
    for service in services {
        let (Some(tlds), Some(urls)) = (
            service.get(0).and_then(Value::as_array),
            service.get(1).and_then(Value::as_array),
        ) else {
            continue;
        };
        let urls: Vec<&str> = urls.iter().filter_map(Value::as_str).collect();
        let Some(url) = urls
            .iter()
            .find(|u| u.starts_with("https://"))
            .or_else(|| urls.first())
        else {
            continue;
        };
        let base = if url.ends_with('/') {
            (*url).to_string()
        } else {
            format!("{url}/")
        };
        for tld in tlds.iter().filter_map(Value::as_str) {
            map.insert(tld.to_ascii_lowercase(), base.clone());
        }
    }
    map
}

fn parse_date(s: &str) -> Option<i64> {
    let s = s.trim();
    OffsetDateTime::parse(s, &Rfc3339)
        .or_else(|_| OffsetDateTime::parse(&format!("{s}Z"), &Rfc3339))
        .or_else(|_| OffsetDateTime::parse(&format!("{s}T00:00:00Z"), &Rfc3339))
        .ok()
        .map(OffsetDateTime::unix_timestamp)
}

fn vcard_fn(entity: &Value) -> Option<String> {
    entity
        .get("vcardArray")?
        .get(1)?
        .as_array()?
        .iter()
        .find(|prop| prop.get(0).and_then(Value::as_str) == Some("fn"))?
        .get(3)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Extract expiration date and registrar from an RDAP domain object.
pub fn parse_rdap(doc: &Value) -> Result<DomainRecord, String> {
    let expires = doc
        .get("events")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|e| {
            e.get("eventAction")
                .and_then(Value::as_str)
                .is_some_and(|a| a.eq_ignore_ascii_case("expiration"))
        })
        .and_then(|e| e.get("eventDate").and_then(Value::as_str))
        .ok_or("The registry does not publish an expiration date for this domain")?;
    let expires_unix =
        parse_date(expires).ok_or_else(|| format!("Unreadable expiration date \"{expires}\""))?;
    let registrar = doc
        .get("entities")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|e| {
            e.get("roles")
                .and_then(Value::as_array)
                .is_some_and(|roles| roles.iter().any(|r| r.as_str() == Some("registrar")))
        })
        .and_then(vcard_fn);
    Ok(DomainRecord {
        expires_unix,
        registrar,
    })
}

/// Build the outcome for a looked-up domain at time `now`.
pub fn outcome_for(domain: &str, record: &DomainRecord, now_unix: i64) -> CheckOutcome {
    let date = util::format_utc(record.expires_unix).unwrap_or_default();
    let day = date.get(..10).unwrap_or(&date).to_string();
    let when = util::relative_days(record.expires_unix, now_unix);
    let registrar = record
        .registrar
        .as_deref()
        .map(|r| format!(" · {}", util::truncate(r, 60)))
        .unwrap_or_default();
    let (status, message) = if record.expires_unix <= now_unix {
        (
            HeartbeatStatus::Down,
            format!("Domain expired {when} ({day}){registrar}"),
        )
    } else {
        (
            HeartbeatStatus::Up,
            format!("Expires {day} ({when}){registrar}"),
        )
    };
    CheckOutcome {
        status,
        response_time_ms: None,
        message: Some(message),
        expiry: (!date.is_empty()).then(|| ExpiryInfo {
            kind: ExpiryKind::Domain,
            subject: Some(domain.to_string()),
            issuer: record.registrar.clone(),
            expires_at: date,
        }),
    }
}

// ---------------------------------------------------------------------------
// Network + caches
// ---------------------------------------------------------------------------

fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(crate::server::USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default()
    })
}

/// A lookup failure; `permanent` ones are cached for [`FAILURE_TTL`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupError {
    pub message: String,
    pub permanent: bool,
}

impl LookupError {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            permanent: false,
        }
    }
    fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            permanent: true,
        }
    }
}

async fn rdap_base_for(tld: &str) -> Result<String, LookupError> {
    struct Bootstrap {
        fetched: Instant,
        map: HashMap<String, String>,
    }
    static BOOTSTRAP: tokio::sync::Mutex<Option<Bootstrap>> = tokio::sync::Mutex::const_new(None);

    // Held across the fetch on purpose: concurrent checks wait for one download.
    let mut guard = BOOTSTRAP.lock().await;
    let stale = guard
        .as_ref()
        .is_none_or(|b| b.fetched.elapsed() > BOOTSTRAP_TTL);
    if stale {
        match fetch_bootstrap().await {
            Ok(map) => {
                *guard = Some(Bootstrap {
                    fetched: Instant::now(),
                    map,
                })
            }
            // Keep using a stale copy rather than failing every check.
            Err(e) if guard.is_none() => return Err(e),
            Err(e) => {
                tracing::warn!(error = %e.message, "RDAP bootstrap refresh failed; using cached copy")
            }
        }
    }
    guard
        .as_ref()
        .and_then(|b| b.map.get(tld).cloned())
        .ok_or_else(|| {
            LookupError::permanent(format!(
                "No RDAP server for .{tld} (this registry does not offer RDAP)"
            ))
        })
}

async fn fetch_bootstrap() -> Result<HashMap<String, String>, LookupError> {
    let fail = |e: reqwest::Error| {
        LookupError::transient(format!(
            "Could not load the IANA RDAP bootstrap: {}",
            util::root_cause(&e)
        ))
    };
    let resp = http()
        .get(BOOTSTRAP_URL)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(fail)?;
    let doc = read_json_capped(resp).await.map_err(|e| {
        LookupError::transient(format!("Could not load the IANA RDAP bootstrap: {e}"))
    })?;
    let map = parse_bootstrap(&doc);
    if map.is_empty() {
        return Err(LookupError::transient(
            "The IANA RDAP bootstrap file was empty",
        ));
    }
    Ok(map)
}

/// Query `{base}domain/{domain}`.
pub async fn lookup_domain(
    client: &reqwest::Client,
    base: &str,
    domain: &str,
) -> Result<DomainRecord, LookupError> {
    let url = format!("{base}domain/{domain}");
    let resp = client
        .get(&url)
        .header(
            reqwest::header::ACCEPT,
            "application/rdap+json, application/json",
        )
        .send()
        .await
        .map_err(|e| {
            LookupError::transient(format!("RDAP request failed: {}", util::root_cause(&e)))
        })?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(LookupError::permanent(format!(
            "{domain} is not registered (RDAP 404)"
        )));
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(LookupError::transient(
            "RDAP server is rate limiting requests (HTTP 429)",
        ));
    }
    if !status.is_success() {
        return Err(LookupError::transient(format!(
            "RDAP server answered HTTP {}",
            status.as_u16()
        )));
    }
    let doc = read_json_capped(resp)
        .await
        .map_err(|e| LookupError::transient(format!("RDAP reply: {e}")))?;
    parse_rdap(&doc).map_err(LookupError::permanent)
}

/// RDAP replies are a few KB; anything past this is refused unread.
const MAX_RDAP_BYTES: usize = 1024 * 1024;

/// Reads a JSON body of at most [`MAX_RDAP_BYTES`].
async fn read_json_capped(mut resp: reqwest::Response) -> Result<Value, String> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("reading failed: {}", util::root_cause(&e)))?
    {
        if buf.len() + chunk.len() > MAX_RDAP_BYTES {
            return Err("larger than 1 MB".into());
        }
        buf.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&buf).map_err(|e| format!("not JSON: {e}"))
}

type CacheEntry = (Instant, Result<DomainRecord, LookupError>);

fn cache() -> &'static Mutex<HashMap<String, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached(domain: &str) -> Option<Result<DomainRecord, LookupError>> {
    let map = cache().lock().ok()?;
    let (at, result) = map.get(domain)?;
    let ttl = if result.is_ok() {
        RESULT_TTL
    } else {
        FAILURE_TTL
    };
    (at.elapsed() < ttl).then(|| result.clone())
}

fn store(domain: &str, result: &Result<DomainRecord, LookupError>) {
    if matches!(result, Err(e) if !e.permanent) {
        return;
    }
    if let Ok(mut map) = cache().lock() {
        map.retain(|_, (at, _)| at.elapsed() < RESULT_TTL);
        map.insert(domain.to_string(), (Instant::now(), result.clone()));
    }
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let domain = match registrable_domain(&monitor.target) {
        Ok(d) => d,
        Err(e) => return CheckOutcome::down(e),
    };
    let start = Instant::now();
    let (result, fetched) = match cached(&domain) {
        Some(r) => (r, false),
        None => {
            let tld = domain.rsplit('.').next().unwrap_or_default().to_string();
            let result = match rdap_base_for(&tld).await {
                Ok(base) => lookup_domain(http(), &base, &domain).await,
                Err(e) => Err(e),
            };
            store(&domain, &result);
            (result, true)
        }
    };
    match result {
        Ok(record) => {
            let mut outcome = outcome_for(&domain, &record, util::now_unix());
            if fetched {
                outcome.response_time_ms = Some(util::elapsed_ms(start));
            }
            outcome
        }
        Err(e) => CheckOutcome::down(e.message),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn registrable_domain_strips_subdomains() {
        assert_eq!(registrable_domain("example.com").unwrap(), "example.com");
        assert_eq!(
            registrable_domain("www.Example.COM.").unwrap(),
            "example.com"
        );
        assert_eq!(
            registrable_domain("https://a.b.example.org:8443/path?q=1").unwrap(),
            "example.org"
        );
        assert_eq!(
            registrable_domain("shop.example.co.uk").unwrap(),
            "example.co.uk"
        );
        assert_eq!(
            registrable_domain("example.com.au").unwrap(),
            "example.com.au"
        );
        assert_eq!(
            registrable_domain("user@mail.example.net").unwrap(),
            "example.net"
        );
        assert_eq!(
            registrable_domain("bücher.example").unwrap(),
            "xn--bcher-kva.example"
        );
    }

    #[test]
    fn registrable_domain_rejects_non_domains() {
        assert!(registrable_domain("localhost").is_err());
        assert!(
            registrable_domain("192.168.1.1")
                .unwrap_err()
                .contains("IP address")
        );
        assert!(
            registrable_domain("co.uk")
                .unwrap_err()
                .contains("public suffix")
        );
        assert!(registrable_domain("").is_err());
    }

    #[test]
    fn bootstrap_parsing_prefers_https() {
        let doc = json!({
            "services": [
                [["com", "net"], ["https://rdap.verisign.com/com/v1/"]],
                [["org"], ["http://rdap.org.example/", "https://rdap.publicinterestregistry.org/rdap"]],
                [["bad"]]
            ]
        });
        let map = parse_bootstrap(&doc);
        assert_eq!(map["com"], "https://rdap.verisign.com/com/v1/");
        assert_eq!(map["net"], "https://rdap.verisign.com/com/v1/");
        assert_eq!(map["org"], "https://rdap.publicinterestregistry.org/rdap/");
        assert!(!map.contains_key("bad"));
    }

    fn rdap_doc(date: &str) -> Value {
        json!({
            "objectClassName": "domain",
            "ldhName": "EXAMPLE.COM",
            "events": [
                {"eventAction": "registration", "eventDate": "1995-08-14T04:00:00Z"},
                {"eventAction": "expiration", "eventDate": date}
            ],
            "entities": [
                {"roles": ["registrar"], "vcardArray": ["vcard", [["version", {}, "text", "4.0"], ["fn", {}, "text", "RESERVED-Internet Assigned Numbers Authority"]]]}
            ]
        })
    }

    #[test]
    fn rdap_parsing() {
        let r = parse_rdap(&rdap_doc("2026-08-13T04:00:00Z")).unwrap();
        assert_eq!(
            util::format_utc(r.expires_unix).as_deref(),
            Some("2026-08-13 04:00:00")
        );
        assert_eq!(
            r.registrar.as_deref(),
            Some("RESERVED-Internet Assigned Numbers Authority")
        );
        // Offsets and missing zones are tolerated.
        let r = parse_rdap(&rdap_doc("2026-08-13T06:00:00+02:00")).unwrap();
        assert_eq!(
            util::format_utc(r.expires_unix).as_deref(),
            Some("2026-08-13 04:00:00")
        );
        assert!(parse_rdap(&rdap_doc("2026-08-13T04:00:00")).is_ok());
        assert!(
            parse_rdap(&json!({"events": []}))
                .unwrap_err()
                .contains("expiration date")
        );
    }

    #[test]
    fn outcome_up_and_expired() {
        let rec = DomainRecord {
            expires_unix: 100 * 86_400,
            registrar: Some("Reg Inc".into()),
        };
        let up = outcome_for("example.com", &rec, 0);
        assert_eq!(up.status, HeartbeatStatus::Up);
        assert_eq!(
            up.message.as_deref(),
            Some("Expires 1970-04-11 (in 100 days) · Reg Inc")
        );
        let exp = up.expiry.unwrap();
        assert_eq!(
            (exp.kind, exp.subject.as_deref(), exp.issuer.as_deref()),
            (ExpiryKind::Domain, Some("example.com"), Some("Reg Inc"))
        );
        assert_eq!(exp.expires_at, "1970-04-11 00:00:00");

        let down = outcome_for("example.com", &rec, 103 * 86_400);
        assert_eq!(down.status, HeartbeatStatus::Down);
        assert_eq!(
            down.message.as_deref(),
            Some("Domain expired 3 days ago (1970-04-11) · Reg Inc")
        );
    }

    #[tokio::test]
    async fn lookup_against_local_rdap_server() {
        use axum::routing::get;
        let app = axum::Router::new().route(
            "/rdap/domain/{name}",
            get(
                |axum::extract::Path(name): axum::extract::Path<String>| async move {
                    if name == "example.com" {
                        axum::Json(rdap_doc("2030-01-01T00:00:00Z")).into_response()
                    } else {
                        axum::http::StatusCode::NOT_FOUND.into_response()
                    }
                },
            ),
        );
        use axum::response::IntoResponse;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let client = reqwest::Client::new();
        let base = format!("http://{addr}/rdap/");
        let rec = lookup_domain(&client, &base, "example.com").await.unwrap();
        assert_eq!(
            util::format_utc(rec.expires_unix).as_deref(),
            Some("2030-01-01 00:00:00")
        );
        let err = lookup_domain(&client, &base, "nope.com").await.unwrap_err();
        assert!(err.permanent && err.message.contains("RDAP 404"));
    }

    #[tokio::test]
    async fn oversized_rdap_reply_is_refused() {
        use axum::body::Body;
        use futures_util::stream;
        // An endless reply: refused once it passes the cap, never buffered whole.
        let app = axum::Router::new().fallback(|| async {
            Body::from_stream(stream::repeat_with(|| Ok::<_, std::io::Error>(vec![b' '; 64 * 1024])))
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let client = reqwest::Client::new();
        let base = format!("http://{addr}/rdap/");
        let err = tokio::time::timeout(Duration::from_secs(10), lookup_domain(&client, &base, "example.com"))
            .await
            .unwrap()
            .unwrap_err();
        assert!(!err.permanent && err.message.contains("1 MB"), "{}", err.message);
    }

    #[tokio::test]
    #[ignore = "needs internet"]
    async fn live_rdap() {
        let m = crate::monitor::checkers::test_support::monitor(
            crate::db::models::MonitorType::DomainExpiry,
            "www.example.com",
        );
        let out = check(&m).await;
        eprintln!("{:?}", out);
        assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
        assert!(out.expiry.is_some());
    }
}
