//! Status page analytics.
//!
//! **Built in, privacy-friendly:** each full page view counts towards a daily
//! total per page, plus the referring domain. No cookies are set and no IP
//! address or user agent is stored: unique visitors are told apart by a
//! SHA-256 of a random salt, the page, the IP and the user agent, kept only
//! in memory; the salt is replaced (and the hashes forgotten) every UTC day,
//! so visitors can't be followed from one day to the next (like Plausible).
//! Bots, prefetches and logged-in users don't count. Counters are kept for
//! max(`retention_days`, 365) days.
//!
//! **Third-party:** Plausible, Umami, Matomo or Google Analytics, with the
//! provider's script on that page only and the page's Content-Security-Policy
//! widened for exactly that provider's origin. Google Analytics and Matomo
//! need a few lines of setup code; that comes from a same-origin script
//! (`/status/{slug}/analytics.js`) so the page never needs inline scripts.

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::{LazyLock, Mutex};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::cookie::SignedCookieJar;
use sha2::{Digest, Sha256};

use crate::api::{ApiError, ApiResult, AppState};
use crate::auth::session;
use crate::db::status_pages::extras::{self, Analytics, DayViews, Provider};
use crate::db::status_pages::{self as db_pages, StatusPage};

/// Counters are kept at least this long.
const MIN_RETENTION_DAYS: i64 = 365;
/// Distinct visitors remembered per day (across pages) before uniques stop
/// being told apart; bounds memory under a flood.
const SEEN_MAX: usize = 500_000;

// ---------------------------------------------------------------------------
// Who counts
// ---------------------------------------------------------------------------

/// User agents that are crawlers, previews, monitors or scripts.
pub fn is_bot(user_agent: &str) -> bool {
    const MARKERS: &[&str] = &[
        "bot", "crawl", "spider", "slurp", "curl", "wget", "python", "httpclient", "http-client", "okhttp", "java/",
        "go-http", "headless", "lighthouse", "pagespeed", "preview", "facebookexternalhit", "embedly", "monitor",
        "uptime", "pingdom", "statuscake", "axios", "node-fetch", "undici", "scrapy", "phantomjs", "selenium",
        "puppeteer", "playwright", "feedfetcher", "vigilant-vole", "libwww", "postman", "insomnia",
    ];
    let ua = user_agent.trim().to_ascii_lowercase();
    ua.is_empty() || !ua.starts_with("mozilla/") || MARKERS.iter().any(|m| ua.contains(m))
}

/// Browser prefetches and prerenders aren't views.
fn is_prefetch(headers: &HeaderMap) -> bool {
    ["purpose", "sec-purpose", "x-purpose", "x-moz"].iter().any(|h| {
        headers
            .get(*h)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("prefetch") || v.to_ascii_lowercase().contains("prerender"))
    })
}

/// The referring site's host, unless it's this site (or not a web page).
pub fn referrer_domain(referer: Option<&str>, own_host: Option<&str>) -> Option<String> {
    let url = url::Url::parse(referer?.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    let own = own_host.map(|h| {
        let h = h.rsplit_once(':').map_or(h, |(h, _)| h).trim_end_matches('.').to_ascii_lowercase();
        h.strip_prefix("www.").map(str::to_string).unwrap_or(h)
    });
    if host.is_empty() || host.len() > 253 || own.as_deref() == Some(host.as_str()) {
        return None;
    }
    Some(host)
}

// ---------------------------------------------------------------------------
// Unique visitors (memory only, salt rotates daily)
// ---------------------------------------------------------------------------

struct Daily {
    day: String,
    salt: [u8; 32],
    seen: HashSet<[u8; 16]>,
}

static DAILY: LazyLock<Mutex<Daily>> = LazyLock::new(|| {
    Mutex::new(Daily {
        day: String::new(),
        salt: rand::random(),
        seen: HashSet::new(),
    })
});

/// `sha256(salt ‖ scope ‖ page ‖ ip ‖ ua)`, truncated. `scope` keeps app
/// instances (tests) apart.
fn visitor_hash(salt: &[u8; 32], scope: &str, page_id: i64, ip: Option<IpAddr>, ua: &str) -> [u8; 16] {
    let mut h = Sha256::new();
    h.update(salt);
    h.update(scope.as_bytes());
    h.update([0]);
    h.update(page_id.to_le_bytes());
    h.update(ip.map(|ip| ip.to_string()).unwrap_or_default().as_bytes());
    h.update([0]);
    h.update(ua.as_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.finalize()[..16]);
    out
}

/// Records a visitor for `day`: `(first visit today, the day just rolled over)`.
/// A new day gets a fresh random salt and forgets every hash of the old one.
fn mark_visitor(day: &str, scope: &str, page_id: i64, ip: Option<IpAddr>, ua: &str) -> (bool, bool) {
    let mut d = DAILY.lock().unwrap_or_else(|e| e.into_inner());
    let rotated = d.day != day;
    if rotated {
        d.day = day.to_string();
        d.salt = rand::random();
        d.seen.clear();
    }
    let hash = visitor_hash(&d.salt, scope, page_id, ip, ua);
    if d.seen.len() >= SEEN_MAX && !d.seen.contains(&hash) {
        return (false, rotated);
    }
    (d.seen.insert(hash), rotated)
}

fn utc_today() -> jiff::civil::Date {
    jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).date()
}

/// Counts a full view of `page` if it should count. Never fails the request.
pub async fn count_view(state: &AppState, page: &StatusPage, headers: &HeaderMap, peer: Option<std::net::SocketAddr>, logged_in: bool) {
    if !page.analytics.enabled || !page.published || logged_in || is_prefetch(headers) {
        return;
    }
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or_default();
    if is_bot(ua) {
        return;
    }
    let today = utc_today();
    let day = today.to_string();
    let scope = state.config.data_dir.to_string_lossy();
    let ip = session::client_ip(headers, peer, session::trust_proxy());
    let (new_visitor, rotated) = mark_visitor(&day, &scope, page.id, ip, ua);
    let new_visitor = new_visitor && page.analytics.unique;
    let own_host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let referrer = referrer_domain(headers.get(header::REFERER).and_then(|v| v.to_str().ok()), own_host);
    let page_id = page.id;
    let result = state
        .with_conn(move |c| {
            extras::record_view(c, page_id, &day, new_visitor, referrer.as_deref())?;
            if rotated {
                let keep = crate::db::settings::load(c)?.retention_days.max(MIN_RETENTION_DAYS);
                let cutoff = today.saturating_sub(jiff::Span::new().days(keep));
                extras::prune_views(c, &cutoff.to_string())?;
            }
            Ok(())
        })
        .await;
    if let Err(err) = result {
        tracing::warn!(%err, page = page_id, "could not count a status page view");
    }
}

// ---------------------------------------------------------------------------
// Third-party snippet + CSP
// ---------------------------------------------------------------------------

/// The one `<script>` a page with a provider gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    pub src: String,
    /// Data attribute the provider reads from its own tag ("" = none).
    pub attr: &'static str,
    pub value: String,
}

pub fn snippet(page: &StatusPage) -> Option<Snippet> {
    let a = &page.analytics;
    match a.provider {
        Provider::None => None,
        Provider::Plausible => Some(Snippet { src: a.url.clone(), attr: "data-domain", value: a.site_id.clone() }),
        Provider::Umami => Some(Snippet { src: a.url.clone(), attr: "data-website-id", value: a.site_id.clone() }),
        Provider::Matomo | Provider::Google => Some(Snippet {
            src: format!("/status/{}/analytics.js?v={}", page.slug, short_hash(&format!("{}|{}", a.site_id, a.url))),
            attr: "",
            value: String::new(),
        }),
    }
}

fn short_hash(s: &str) -> String {
    hex::encode(&Sha256::digest(s.as_bytes())[..6])
}

/// The public-page CSP (same as the default one in `api::security_headers`)
/// with the provider's origins added to `script-src`, `connect-src` and
/// `img-src`. `None` without a provider (the default policy applies).
pub fn csp(a: &Analytics) -> Option<String> {
    let (script, connect, img) = a.csp_sources()?;
    let join = |extra: &[String]| extra.iter().map(|s| format!(" {s}")).collect::<String>();
    Some(format!(
        "default-src 'self'; script-src 'self'{}; style-src 'self' 'unsafe-inline'; img-src 'self' data:{}; \
         connect-src 'self'{}; font-src 'self'; base-uri 'self'; form-action 'self'",
        join(&script),
        join(&img),
        join(&connect)
    ))
}

/// Setup code for providers that need some (GA, Matomo). Values are
/// validated already and JSON-encoded on top.
pub fn setup_script(a: &Analytics) -> Option<String> {
    let id = serde_json::to_string(&a.site_id).ok()?;
    match a.provider {
        Provider::Google => Some(format!(
            "window.dataLayer = window.dataLayer || [];\n\
             function gtag() {{ window.dataLayer.push(arguments); }}\n\
             gtag('js', new Date());\n\
             gtag('config', {id});\n\
             (function () {{\n  var s = document.createElement('script');\n  s.async = true;\n  \
             s.src = 'https://www.googletagmanager.com/gtag/js?id=' + encodeURIComponent({id});\n  \
             document.head.appendChild(s);\n}})();\n"
        )),
        Provider::Matomo => {
            let base = serde_json::to_string(&a.url).ok()?;
            Some(format!(
                "var _paq = window._paq = window._paq || [];\n\
                 _paq.push(['trackPageView']);\n\
                 _paq.push(['enableLinkTracking']);\n\
                 (function () {{\n  var u = {base};\n  _paq.push(['setTrackerUrl', u + 'matomo.php']);\n  \
                 _paq.push(['setSiteId', {id}]);\n  var s = document.createElement('script');\n  s.async = true;\n  \
                 s.src = u + 'matomo.js';\n  document.head.appendChild(s);\n}})();\n"
            ))
        }
        _ => None,
    }
}

/// `GET /status/{slug}/analytics.js`.
pub async fn analytics_js(State(state): State<AppState>, jar: SignedCookieJar, Path(slug): Path<String>) -> ApiResult<Response> {
    let page = state
        .with_conn(move |c| db_pages::get_by_slug(c, &slug))
        .await?
        .ok_or(ApiError::NotFound)?;
    if !page.published && crate::api::auth::session_user(&state, &jar).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    let js = setup_script(&page.analytics).ok_or(ApiError::NotFound)?;
    let mut resp = (StatusCode::OK, js).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/javascript; charset=utf-8"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=300"));
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Editor: "Views last 30 days"
// ---------------------------------------------------------------------------

/// Days in the editor chart.
pub const CHART_DAYS: i64 = 30;

/// The SVG chart of daily views (design-system `.chart`).
#[derive(Debug, Clone, Default)]
pub struct ViewsChart {
    pub width: i64,
    pub height: i64,
    pub mid_y: i64,
    pub line: String,
    pub area: String,
    pub max_label: String,
    pub start_label: String,
    pub aria_label: String,
}

/// Editor summary of a page's built-in analytics.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub views: String,
    pub visitors: String,
    pub today: String,
    pub has_data: bool,
    pub chart: ViewsChart,
    /// (domain, views label).
    pub referrers: Vec<(String, String)>,
}

pub fn chart(days: &[DayViews]) -> ViewsChart {
    let (width, height) = (600_i64, 120_i64);
    let max = days.iter().map(|d| d.views).max().unwrap_or(0).max(1);
    let n = days.len().max(2) as f64 - 1.0;
    let points: Vec<String> = days
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let x = i as f64 * width as f64 / n;
            let y = height as f64 - d.views as f64 / max as f64 * (height - 8) as f64;
            format!("{x:.1},{y:.1}")
        })
        .collect();
    let line = points.join(" ");
    let area = format!("{line} {width},{height} 0,{height}");
    let peak = days.iter().map(|d| d.views).max().unwrap_or(0);
    ViewsChart {
        width,
        height,
        mid_y: height / 2 + 4,
        line,
        area,
        max_label: crate::i18n::fmt_int(max),
        start_label: days
            .first()
            .and_then(|d| d.day.parse::<jiff::civil::Date>().ok())
            .map(crate::i18n::fmt_day_month)
            .unwrap_or_default(),
        aria_label: crate::i18n::tf("spx.chart_aria", &[("n", &crate::i18n::fmt_int(peak))]),
    }
}

pub fn load_stats(conn: &rusqlite::Connection, page_id: i64) -> rusqlite::Result<Stats> {
    let today = utc_today();
    let days = extras::daily_views(conn, page_id, today, CHART_DAYS)?;
    let since = days.first().map(|d| d.day.clone()).unwrap_or_default();
    let referrers = extras::top_referrers(conn, page_id, &since, 8)?
        .into_iter()
        .map(|(d, n)| (d, crate::i18n::fmt_int(n)))
        .collect();
    let views: i64 = days.iter().map(|d| d.views).sum();
    Ok(Stats {
        views: crate::i18n::fmt_int(views),
        visitors: crate::i18n::fmt_int(days.iter().map(|d| d.visitors).sum()),
        today: crate::i18n::fmt_int(days.last().map_or(0, |d| d.views)),
        has_data: views > 0,
        chart: chart(&days),
        referrers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIREFOX: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:131.0) Gecko/20100101 Firefox/131.0";

    #[test]
    fn bots_scripts_and_monitors_are_excluded() {
        assert!(!is_bot(FIREFOX));
        assert!(!is_bot("Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 Mobile/15E148 Safari/604.1"));
        for ua in [
            "",
            "curl/8.4.0",
            "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
            "Mozilla/5.0 (Windows NT 10.0) HeadlessChrome/120.0",
            "Uptime-Kuma/2.0.0",
            "Mozilla/5.0 (compatible; UptimeRobot/2.0)",
            "python-requests/2.31",
            "facebookexternalhit/1.1",
            "Mozilla/5.0 (Linux) Chrome-Lighthouse",
        ] {
            assert!(is_bot(ua), "{ua}");
        }
    }

    #[test]
    fn prefetches_dont_count() {
        let mut h = HeaderMap::new();
        assert!(!is_prefetch(&h));
        h.insert("sec-purpose", HeaderValue::from_static("prefetch;prerender"));
        assert!(is_prefetch(&h));
    }

    #[test]
    fn referrers_are_foreign_domains_only() {
        assert_eq!(referrer_domain(Some("https://www.News.example/a?b"), Some("status.acme.test")).as_deref(), Some("news.example"));
        assert_eq!(referrer_domain(Some("https://status.acme.test/x"), Some("status.acme.test:443")), None);
        assert_eq!(referrer_domain(Some("android-app://com.slack"), None), None);
        assert_eq!(referrer_domain(Some("not a url"), None), None);
        assert_eq!(referrer_domain(None, None), None);
    }

    #[test]
    fn unique_visitor_hashes_rotate_daily() {
        let scope = "unit-test-rotation";
        let ip: Option<IpAddr> = "203.0.113.7".parse().ok();
        let (first, _) = mark_visitor("2001-01-01", scope, 1, ip, FIREFOX);
        assert!(first);
        assert!(!mark_visitor("2001-01-01", scope, 1, ip, FIREFOX).0, "same visitor, same day");
        assert!(mark_visitor("2001-01-01", scope, 2, ip, FIREFOX).0, "another page counts separately");
        assert!(mark_visitor("2001-01-01", scope, 1, ip, "Mozilla/5.0 other").0, "another browser is another visitor");
        let (again, rotated) = mark_visitor("2001-01-02", scope, 1, ip, FIREFOX);
        assert!(again && rotated, "a new day forgets yesterday's visitors");
        // Same inputs, different salt → unlinkable hashes.
        assert_ne!(visitor_hash(&[1; 32], scope, 1, ip, FIREFOX), visitor_hash(&[2; 32], scope, 1, ip, FIREFOX));
    }

    #[test]
    fn csp_only_widens_for_the_configured_provider() {
        assert_eq!(csp(&Analytics::default()), None);
        let a = Analytics {
            provider: Provider::Plausible,
            site_id: "example.com".into(),
            url: "https://plausible.io/js/script.js".into(),
            ..Analytics::default()
        };
        let policy = csp(&a).unwrap();
        assert!(policy.contains("script-src 'self' https://plausible.io;"), "{policy}");
        assert!(policy.contains("connect-src 'self' https://plausible.io;"), "{policy}");
        assert!(policy.contains("img-src 'self' data:;"), "{policy}");
        assert!(!policy.contains("googletagmanager"));
        assert!(!policy.contains("unsafe-eval") && !policy.contains("script-src 'self' 'unsafe-inline'"));
    }

    #[test]
    fn setup_scripts_encode_values() {
        let ga = Analytics { provider: Provider::Google, site_id: "G-ABCDEF12".into(), ..Analytics::default() };
        let js = setup_script(&ga).unwrap();
        assert!(js.contains("gtag('config', \"G-ABCDEF12\")"));
        assert!(js.contains("https://www.googletagmanager.com/gtag/js?id="));
        let m = Analytics {
            provider: Provider::Matomo,
            site_id: "3".into(),
            url: "https://m.example.com/".into(),
            ..Analytics::default()
        };
        let js = setup_script(&m).unwrap();
        assert!(js.contains("var u = \"https://m.example.com/\";"));
        assert!(js.contains("_paq.push(['setSiteId', \"3\"]);"));
        assert_eq!(setup_script(&Analytics::default()), None);
    }

    #[test]
    fn chart_scales_to_the_peak() {
        let days: Vec<DayViews> = (1..=30)
            .map(|i| DayViews { day: format!("2026-10-{i:02}"), views: if i == 30 { 10 } else { 0 }, visitors: 0 })
            .collect();
        let c = chart(&days);
        assert!(c.line.starts_with("0.0,120.0"));
        assert!(c.line.ends_with("600.0,8.0"));
        assert_eq!(c.max_label, "10");
        assert!(c.area.ends_with("600,120 0,120"));
    }
}
