//! Real-browser checker: loads the target in a remote headless Chromium over
//! the Chrome DevTools Protocol (see [`cdp`] and [`endpoint`]), so this
//! image needs no browser of its own.
//!
//! Per check: a fresh browser context (`Target.createBrowserContext`, so
//! cookies and storage never leak between checks) with one page, attached
//! as a flattened session; viewport, extra headers (the monitor's headers
//! plus basic auth) and TLS leniency are set; `Page.navigate`; then wait for
//! the main frame's `load` or `networkIdle` lifecycle event. The main
//! document's status comes from `Network.responseReceived`, keywords and CSS
//! selectors are evaluated in the page, and uncaught exceptions
//! (`Runtime.exceptionThrown`) can fail the check. On failure a JPEG of the
//! viewport is captured and stored as the monitor's latest screenshot. The
//! page and its context are always closed afterwards, in a spawned task, so
//! that also happens when the check is cut off by its timeout.
//!
//! Response time = start of the document request → `load` event.
//! Each remote browser runs at most [`PER_BROWSER_CHECKS`] checks at once.
//!
//! The checker reads the remote browser from the database installed with
//! [`install`] (the server does that at startup). Remote agents have none
//! and never get these monitors (see `MonitorType::runs_on_agents`).

pub mod cdp;
pub mod endpoint;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Map, Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::timeout_at;

use self::cdp::{CdpError, Connection, Event, Events};
use super::{StatusCodes, parse_headers, parse_status_codes, util};
use crate::db::models::{CheckOutcome, Monitor};
use crate::db::remote_browsers::{self as db_rb, MAX_SCREENSHOT_BYTES};

/// Values of `monitors.browser_wait`.
pub const WAITS: &[&str] = &["load", "network_idle"];
/// Values of `monitors.browser_viewport`.
pub const VIEWPORTS: &[&str] = &["desktop", "mobile"];
/// Checks one remote browser runs at the same time; more wait their turn.
pub const PER_BROWSER_CHECKS: usize = 3;
/// How long closing the page and context may take after a check.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const JPEG_QUALITY: i64 = 70;
const SMALLER_JPEG_QUALITY: i64 = 40;

/// Shown when no remote browser is configured.
pub const NO_BROWSER: &str = "No remote browser configured — add one in Settings → Remote browsers";

static DB: OnceLock<deadpool_sqlite::Pool> = OnceLock::new();

/// Lets real-browser checks read remote browsers and store screenshots.
pub fn install(pool: deadpool_sqlite::Pool) {
    let _ = DB.set(pool);
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    Load,
    NetworkIdle,
}

impl Wait {
    pub fn parse(s: &str) -> Self {
        if s == "network_idle" {
            Wait::NetworkIdle
        } else {
            Wait::Load
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Viewport {
    /// 1280 × 800.
    Desktop,
    /// 390 × 844 at 2× (a current phone), touch and mobile layout.
    Mobile,
}

impl Viewport {
    pub fn parse(s: &str) -> Self {
        if s == "mobile" {
            Viewport::Mobile
        } else {
            Viewport::Desktop
        }
    }

    fn metrics(self) -> Value {
        match self {
            Viewport::Desktop => {
                json!({"width": 1280, "height": 800, "deviceScaleFactor": 1, "mobile": false})
            }
            Viewport::Mobile => {
                json!({"width": 390, "height": 844, "deviceScaleFactor": 2, "mobile": true})
            }
        }
    }
}

/// What the page must (or, inverted, must not) contain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    Nothing,
    /// Text in `document.body.innerText` (case-sensitive).
    Keyword(String),
    /// `document.querySelector(…)` matches.
    Selector(String),
}

/// One check's settings, from the monitor.
#[derive(Debug, Clone)]
pub struct Options {
    pub url: String,
    pub accepted: StatusCodes,
    pub expect: Expect,
    pub invert: bool,
    pub js_errors: bool,
    pub wait: Wait,
    pub viewport: Viewport,
    pub headers: Vec<(String, String)>,
    pub ignore_tls: bool,
}

impl Options {
    pub fn from_monitor(m: &Monitor) -> Result<Self, String> {
        let url = url::Url::parse(m.target.trim())
            .map_err(|_| format!("Invalid URL \"{}\"", util::truncate(&m.target, 80)))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("Only http:// and https:// pages can be checked".into());
        }
        // The browser resolves names itself; a literal address is checked here.
        if let Some(host) = url.host_str() {
            crate::kit::net::check_host(host).map_err(|e| format!("Refused: {e}"))?;
        }
        let accepted = parse_status_codes(&m.accepted_status_codes)
            .map_err(|e| format!("Accepted status codes: {e}"))?;
        let mut headers: Vec<(String, String)> =
            parse_headers(m.http_headers.as_deref().unwrap_or_default())?
                .iter()
                .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
                .collect();
        if m.auth_method == "basic"
            && let Some(user) = m.basic_auth_user.as_deref().filter(|u| !u.is_empty())
        {
            let raw = format!(
                "{user}:{}",
                m.basic_auth_pass.as_deref().unwrap_or_default()
            );
            headers.retain(|(k, _)| !k.eq_ignore_ascii_case("authorization"));
            headers.push((
                "Authorization".into(),
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(raw)
                ),
            ));
        }
        let expect = match (
            m.browser_selector.as_deref().map(str::trim),
            m.keyword.as_deref(),
        ) {
            (Some(sel), _) if !sel.is_empty() => Expect::Selector(sel.to_string()),
            (_, Some(k)) if !k.is_empty() => Expect::Keyword(k.to_string()),
            _ => Expect::Nothing,
        };
        Ok(Options {
            url: url.to_string(),
            accepted,
            expect,
            invert: m.keyword_invert,
            js_errors: m.browser_js_errors,
            wait: Wait::parse(&m.browser_wait),
            viewport: Viewport::parse(&m.browser_viewport),
            headers,
            ignore_tls: m.ignore_tls,
        })
    }
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let Some(pool) = DB.get() else {
        return CheckOutcome::down(
            "Real-browser checks run on the server, which holds the remote browser settings",
        );
    };
    let wanted = monitor.remote_browser_id;
    let browser = match crate::db::interact(pool, move |c| db_rb::for_monitor(c, wanted)).await {
        Ok(Some(b)) => b,
        Ok(None) => return CheckOutcome::down(NO_BROWSER),
        Err(err) => {
            return CheckOutcome::down(format!("Couldn't read the remote browser settings: {err}"));
        }
    };
    let opts = match Options::from_monitor(monitor) {
        Ok(o) => o,
        Err(e) => return CheckOutcome::down(e),
    };
    let run = run(&browser.url, &opts, super::timeout_of(monitor)).await;
    if let Some(jpeg) = run.screenshot {
        // Stored on its own task: the result doesn't wait, and a check cut
        // off by its timeout can't cut the write off half-way.
        let (pool, id, message) = (pool.clone(), monitor.id, run.outcome.message.clone());
        tokio::spawn(async move {
            let saved = crate::db::interact(&pool, move |c| {
                db_rb::save_screenshot(c, id, &jpeg, "image/jpeg", message.as_deref())
            })
            .await;
            if let Err(err) = saved {
                tracing::warn!(monitor_id = id, %err, "couldn't store the failure screenshot");
            }
        });
    }
    run.outcome
}

/// What [`test_connection`] learned about a browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserVersion {
    /// e.g. `HeadlessChrome/131.0.6778.85`.
    pub product: String,
    pub protocol: String,
    pub user_agent: String,
}

/// Connects and asks `Browser.getVersion` ("Test connection").
pub async fn test_connection(url: &str, timeout: Duration) -> Result<BrowserVersion, String> {
    let deadline = Instant::now() + timeout;
    let endpoint = endpoint::parse(url)?;
    let target = timeout_at(deadline.into(), endpoint::resolve(&endpoint, timeout))
        .await
        .map_err(|_| format!("no answer within {}", util::format_secs(timeout)))??;
    let (conn, _events) = timeout_at(deadline.into(), endpoint::connect(&target, |_, _| false))
        .await
        .map_err(|_| format!("no answer within {}", util::format_secs(timeout)))??;
    let v = timeout_at(
        deadline.into(),
        conn.call("Browser.getVersion", Value::Null, None),
    )
    .await
    .map_err(|_| "the browser didn't answer Browser.getVersion".to_string())?
    .map_err(|e| e.to_string())?;
    let text = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Ok(BrowserVersion {
        product: text("product"),
        protocol: text("protocolVersion"),
        user_agent: text("userAgent"),
    })
}

/// One check's result and, when it failed, a JPEG of the page.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub outcome: CheckOutcome,
    pub screenshot: Option<Vec<u8>>,
}

impl Run {
    fn down(message: impl Into<String>) -> Self {
        Run {
            outcome: CheckOutcome::down(message),
            screenshot: None,
        }
    }
}

/// Per-browser check slots, by endpoint URL.
static SLOTS: LazyLock<Mutex<HashMap<String, Arc<Semaphore>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn slots_for(endpoint: &str) -> Arc<Semaphore> {
    SLOTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(endpoint.trim().to_string())
        .or_insert_with(|| Arc::new(Semaphore::new(PER_BROWSER_CHECKS)))
        .clone()
}

/// Events the checker looks at; the rest (most of the network chatter) is
/// dropped by the reader.
fn interesting(method: &str, params: &Value) -> bool {
    match method {
        "Page.lifecycleEvent"
        | "Runtime.exceptionThrown"
        | "Inspector.targetCrashed"
        | "Inspector.detached"
        | "Target.detachedFromTarget"
        | "Target.targetCrashed" => true,
        "Network.requestWillBeSent" | "Network.responseReceived" => {
            params.get("type").and_then(Value::as_str) == Some("Document")
        }
        _ => false,
    }
}

/// Runs one check against the browser at `endpoint`, within `budget`.
pub async fn run(endpoint: &str, opts: &Options, budget: Duration) -> Run {
    let start = Instant::now();
    // Finish a little before the caller's timeout so the result is ours.
    let margin = (budget / 10).clamp(Duration::from_millis(200), Duration::from_secs(1));
    let deadline = start + budget.saturating_sub(margin);
    // Keep time for a screenshot after a page that doesn't finish loading.
    let reserve = (budget / 4).min(Duration::from_secs(2));
    let load_deadline = deadline.checked_sub(reserve).unwrap_or(deadline).max(start);

    let url = match endpoint::parse(endpoint) {
        Ok(u) => u,
        Err(e) => return Run::down(format!("Remote browser address is invalid: {e}")),
    };
    let permit = match timeout_at(deadline.into(), slots_for(endpoint).acquire_owned()).await {
        Ok(Ok(p)) => p,
        _ => {
            return Run::down(format!(
                "Remote browser busy: {PER_BROWSER_CHECKS} checks were already running on it"
            ));
        }
    };
    let unreachable = |why: String| Run::down(format!("Remote browser unreachable: {why}"));
    let target = match timeout_at(
        deadline.into(),
        endpoint::resolve(&url, deadline.saturating_duration_since(Instant::now())),
    )
    .await
    {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => return unreachable(e),
        Err(_) => return unreachable("no answer in time".into()),
    };
    let (conn, events) =
        match timeout_at(deadline.into(), endpoint::connect(&target, interesting)).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => return unreachable(e),
            Err(_) => return unreachable("no answer in time".into()),
        };
    let mut cleanup = Cleanup {
        conn: Some(conn.clone()),
        context: None,
        target: None,
        permit: Some(permit),
    };
    let page = match timeout_at(deadline.into(), open_page(&conn, &mut cleanup)).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => return Run::down(format!("Remote browser error: {e}")),
        Err(_) => return Run::down("Remote browser error: it didn't open a page in time"),
    };
    let run = drive(
        &conn,
        events,
        &page,
        opts,
        Deadlines {
            start,
            load: load_deadline,
            end: deadline,
        },
    )
    .await;
    drop(cleanup);
    run
}

// ---------------------------------------------------------------------------
// Page lifecycle
// ---------------------------------------------------------------------------

struct Page {
    target_id: String,
    session_id: String,
}

/// Closes the page and disposes of its context when dropped — after a
/// finished check and when the check is aborted by its timeout alike. The
/// browser slot is released once that is done.
struct Cleanup {
    conn: Option<Connection>,
    context: Option<String>,
    target: Option<String>,
    permit: Option<OwnedSemaphorePermit>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let (Some(conn), permit) = (self.conn.take(), self.permit.take()) else {
            return;
        };
        let (target, context) = (self.target.take(), self.context.take());
        if target.is_none() && context.is_none() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            let _permit = permit;
            let close = async {
                if let Some(t) = target {
                    let _ = conn
                        .call("Target.closeTarget", json!({"targetId": t}), None)
                        .await;
                }
                if let Some(c) = context {
                    let _ = conn
                        .call(
                            "Target.disposeBrowserContext",
                            json!({"browserContextId": c}),
                            None,
                        )
                        .await;
                }
            };
            let _ = tokio::time::timeout(CLEANUP_TIMEOUT, close).await;
        });
    }
}

fn field(v: &Value, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("the browser's answer has no {key}"))
}

async fn open_page(conn: &Connection, cleanup: &mut Cleanup) -> Result<Page, String> {
    let ctx = conn
        .call(
            "Target.createBrowserContext",
            json!({"disposeOnDetach": true}),
            None,
        )
        .await
        .map_err(|e| e.to_string())?;
    let context_id = field(&ctx, "browserContextId")?;
    cleanup.context = Some(context_id.clone());
    let created = conn
        .call(
            "Target.createTarget",
            json!({"url": "about:blank", "browserContextId": context_id}),
            None,
        )
        .await
        .map_err(|e| e.to_string())?;
    let target_id = field(&created, "targetId")?;
    cleanup.target = Some(target_id.clone());
    let attached = conn
        .call(
            "Target.attachToTarget",
            json!({"targetId": target_id, "flatten": true}),
            None,
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(Page {
        session_id: field(&attached, "sessionId")?,
        target_id,
    })
}

#[derive(Clone, Copy)]
struct Deadlines {
    start: Instant,
    /// Stop waiting for the page.
    load: Instant,
    /// Stop everything (screenshot included).
    end: Instant,
}

/// What the events said about the navigation.
#[derive(Debug, Default)]
struct PageState {
    frame_id: String,
    /// The main frame's current document (a client-side redirect replaces it).
    loader_id: String,
    /// Monotonic seconds: when the document request started.
    request_ts: Option<f64>,
    load_ts: Option<f64>,
    load_at: Option<Instant>,
    idle: bool,
    /// Main document responses by loader id: (status, status text).
    responses: HashMap<String, (u16, String)>,
    navigation_loader: String,
    exceptions: Vec<String>,
    fatal: Option<String>,
}

impl PageState {
    fn apply(&mut self, ev: &Event, page: &Page) {
        let p = &ev.params;
        let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or_default();
        if ev.session_id.as_deref() != Some(page.session_id.as_str()) {
            match ev.method.as_str() {
                "Target.detachedFromTarget" if s("sessionId") == page.session_id => {
                    self.fatal
                        .get_or_insert_with(|| "The browser closed the page".into());
                }
                "Target.targetCrashed" if s("targetId") == page.target_id => {
                    self.fatal.get_or_insert_with(|| "The page crashed".into());
                }
                _ => {}
            }
            return;
        }
        match ev.method.as_str() {
            "Page.lifecycleEvent" if s("frameId") == self.frame_id => {
                let loader = s("loaderId");
                match s("name") {
                    "init" if !loader.is_empty() && loader != self.loader_id => {
                        // A new document in the main frame (e.g. a JavaScript redirect).
                        self.loader_id = loader.to_string();
                        self.load_at = None;
                        self.load_ts = None;
                        self.idle = false;
                    }
                    "load" if loader == self.loader_id && self.load_at.is_none() => {
                        self.load_at = Some(Instant::now());
                        self.load_ts = p.get("timestamp").and_then(Value::as_f64);
                    }
                    "networkIdle" if loader == self.loader_id => self.idle = true,
                    _ => {}
                }
            }
            "Network.requestWillBeSent"
                if s("frameId") == self.frame_id && s("loaderId") == self.navigation_loader =>
            {
                if self.request_ts.is_none() {
                    self.request_ts = p.get("timestamp").and_then(Value::as_f64);
                }
            }
            "Network.responseReceived" if s("frameId") == self.frame_id => {
                let r = &p["response"];
                if let Some(status) = r.get("status").and_then(Value::as_u64) {
                    let text = r
                        .get("statusText")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    self.responses.insert(
                        s("loaderId").to_string(),
                        (u16::try_from(status).unwrap_or(0), text),
                    );
                }
            }
            "Runtime.exceptionThrown" => {
                let d = &p["exceptionDetails"];
                let text = d
                    .pointer("/exception/description")
                    .and_then(Value::as_str)
                    .or_else(|| d.get("text").and_then(Value::as_str))
                    .unwrap_or("Uncaught exception");
                let first = text.lines().next().unwrap_or(text).trim();
                self.exceptions.push(util::truncate(first, 200));
            }
            "Inspector.targetCrashed" => {
                self.fatal.get_or_insert_with(|| "The page crashed".into());
            }
            "Inspector.detached" => {
                self.fatal
                    .get_or_insert_with(|| "The browser closed the page".into());
            }
            _ => {}
        }
    }

    fn reached(&self, wait: Wait) -> bool {
        match wait {
            Wait::Load => self.load_at.is_some(),
            Wait::NetworkIdle => self.load_at.is_some() && self.idle,
        }
    }

    /// Start of the document request → load (browser clock when known).
    fn load_ms(&self, nav_start: Instant) -> Option<i64> {
        match (self.request_ts, self.load_ts) {
            (Some(a), Some(b)) if b >= a => Some(((b - a) * 1000.0).round() as i64),
            _ => self.load_at.map(|t| {
                i64::try_from(t.saturating_duration_since(nav_start).as_millis())
                    .unwrap_or(i64::MAX)
            }),
        }
    }
}

fn secs(ms: i64) -> String {
    format!("{:.2} s", ms as f64 / 1000.0)
}

fn http_status(code: u16, text: &str) -> String {
    let reason = http::StatusCode::from_u16(code)
        .ok()
        .and_then(|s| s.canonical_reason())
        .map(str::to_string);
    match reason.or_else(|| (!text.trim().is_empty()).then(|| text.trim().to_string())) {
        Some(r) => format!("HTTP {code} {r}"),
        None => format!("HTTP {code}"),
    }
}

/// Drives the page through one check.
async fn drive(
    conn: &Connection,
    mut events: Events,
    page: &Page,
    opts: &Options,
    d: Deadlines,
) -> Run {
    let session = Some(page.session_id.as_str());
    let call = |method: &'static str, params: Value| conn.call(method, params, session);
    let setup = async {
        call("Page.enable", json!({})).await?;
        call("Page.setLifecycleEventsEnabled", json!({"enabled": true})).await?;
        call("Runtime.enable", json!({})).await?;
        call("Network.enable", json!({})).await?;
        call(
            "Emulation.setDeviceMetricsOverride",
            opts.viewport.metrics(),
        )
        .await?;
        if opts.viewport == Viewport::Mobile {
            call(
                "Emulation.setTouchEmulationEnabled",
                json!({"enabled": true, "maxTouchPoints": 5}),
            )
            .await?;
        }
        if !opts.headers.is_empty() {
            let headers: Map<String, Value> = opts
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), json!(v)))
                .collect();
            call("Network.setExtraHTTPHeaders", json!({"headers": headers})).await?;
        }
        if opts.ignore_tls {
            call(
                "Security.setIgnoreCertificateErrors",
                json!({"ignore": true}),
            )
            .await?;
        }
        Ok::<(), CdpError>(())
    };
    match timeout_at(d.load.into(), setup).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Run::down(format!("Remote browser error: {e}")),
        Err(_) => return Run::down("Remote browser error: it didn't set up the page in time"),
    }
    // Lifecycle events of the blank page (replayed when they were enabled)
    // have all arrived by now; they aren't about the page being checked.
    while events.try_next().is_some() {}

    let nav_start = Instant::now();
    let nav = match timeout_at(
        d.load.into(),
        call("Page.navigate", json!({"url": opts.url})),
    )
    .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(CdpError::Closed)) => {
            return Run::down("Remote browser closed the connection during navigation");
        }
        Ok(Err(e)) => return Run::down(format!("Navigation failed: {e}")),
        Err(_) => {
            let msg = format!(
                "No response from the page within {}",
                util::format_secs(d.load.saturating_duration_since(d.start))
            );
            return finish_down(conn, page, msg, None, d).await;
        }
    };
    if let Some(err) = nav
        .get("errorText")
        .and_then(Value::as_str)
        .filter(|e| !e.is_empty())
    {
        return finish_down(conn, page, format!("Navigation failed: {err}"), None, d).await;
    }
    let mut st = PageState {
        frame_id: nav
            .get("frameId")
            .and_then(Value::as_str)
            .unwrap_or(&page.target_id)
            .to_string(),
        loader_id: nav
            .get("loaderId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        ..PageState::default()
    };
    st.navigation_loader = st.loader_id.clone();

    let mut closed = false;
    while !st.reached(opts.wait) && st.fatal.is_none() {
        match timeout_at(d.load.into(), events.next()).await {
            Ok(Some(ev)) => st.apply(&ev, page),
            Ok(None) => {
                closed = true;
                break;
            }
            Err(_) => break,
        }
    }
    while let Some(ev) = events.try_next() {
        st.apply(&ev, page);
    }
    if closed && st.fatal.is_none() {
        return Run::down("Remote browser closed the connection while the page loaded");
    }
    let load_ms = st.load_ms(nav_start);
    if let Some(fatal) = st.fatal.clone() {
        return Run::down(fatal);
    }

    let status = st
        .responses
        .get(&st.navigation_loader)
        .or_else(|| st.responses.get(&st.loader_id))
        .cloned();
    if let Some((code, text)) = &status
        && !opts.accepted.contains(*code)
    {
        return finish_down(conn, page, http_status(*code, text), load_ms, d).await;
    }
    if !st.reached(opts.wait) {
        let waited = util::format_secs(d.load.saturating_duration_since(nav_start));
        let msg = match (opts.wait, load_ms) {
            (Wait::NetworkIdle, Some(ms)) => format!(
                "Network didn't go idle within {waited} (loaded in {})",
                secs(ms)
            ),
            _ => format!("Page didn't finish loading within {waited}"),
        };
        return finish_down(conn, page, msg, load_ms, d).await;
    }
    if opts.js_errors
        && let Some(first) = st.exceptions.first()
    {
        let more = match st.exceptions.len() {
            1 => String::new(),
            n => format!(" (+{} more)", n - 1),
        };
        return finish_down(
            conn,
            page,
            format!("JavaScript error: {first}{more}"),
            load_ms,
            d,
        )
        .await;
    }
    let found = match evaluate(conn, page, &opts.expect, opts.invert, d.end).await {
        Ok(found) => found,
        Err(msg) => return finish_down(conn, page, msg, load_ms, d).await,
    };

    let ms = load_ms.unwrap_or_else(|| util::elapsed_ms(nav_start));
    let mut message = match &status {
        Some((code, _)) => format!("HTTP {code} · loaded in {}", secs(ms)),
        None => format!("Loaded in {}", secs(ms)),
    };
    if let Some(found) = found {
        message.push_str(" · ");
        message.push_str(&found);
    }
    Run {
        outcome: CheckOutcome::up(ms).with_message(message),
        screenshot: None,
    }
}

/// Captures the page for a failed check.
async fn finish_down(
    conn: &Connection,
    page: &Page,
    message: String,
    load_ms: Option<i64>,
    d: Deadlines,
) -> Run {
    let mut outcome = CheckOutcome::down(message);
    outcome.response_time_ms = load_ms;
    let screenshot = screenshot(conn, page, d.end).await;
    Run {
        outcome,
        screenshot,
    }
}

async fn screenshot(conn: &Connection, page: &Page, deadline: Instant) -> Option<Vec<u8>> {
    for quality in [JPEG_QUALITY, SMALLER_JPEG_QUALITY] {
        let shot = conn.call(
            "Page.captureScreenshot",
            json!({"format": "jpeg", "quality": quality}),
            Some(&page.session_id),
        );
        let data = timeout_at(deadline.into(), shot).await.ok()?.ok()?;
        let jpeg = base64::engine::general_purpose::STANDARD
            .decode(data.get("data")?.as_str()?)
            .ok()?;
        if jpeg.len() <= MAX_SCREENSHOT_BYTES {
            return Some(jpeg);
        }
    }
    None
}

/// The JavaScript that looks for a keyword in the page text.
pub fn keyword_script(keyword: &str) -> String {
    let k = serde_json::to_string(keyword).unwrap_or_else(|_| "\"\"".into());
    format!(
        "(() => {{ const t = (document.body && document.body.innerText) || ''; const i = t.indexOf({k}); \
         return {{ i, head: t.slice(0, 200), around: i >= 0 ? t.slice(Math.max(0, i - 30), i + 120) : '' }}; }})()"
    )
}

/// The JavaScript that tries a CSS selector.
pub fn selector_script(selector: &str) -> String {
    let s = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into());
    format!(
        "(() => {{ try {{ return {{ found: document.querySelector({s}) !== null }}; }} \
         catch (e) {{ return {{ error: String((e && e.message) || e) }}; }} }})()"
    )
}

/// `Ok(Some(note))` / `Ok(None)` when it passes, `Err(down message)` when not.
async fn evaluate(
    conn: &Connection,
    page: &Page,
    expect: &Expect,
    invert: bool,
    deadline: Instant,
) -> Result<Option<String>, String> {
    let script = match expect {
        Expect::Nothing => return Ok(None),
        Expect::Keyword(k) => keyword_script(k),
        Expect::Selector(s) => selector_script(s),
    };
    let call = conn.call(
        "Runtime.evaluate",
        json!({"expression": script, "returnByValue": true}),
        Some(&page.session_id),
    );
    let v = match timeout_at(deadline.into(), call).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(format!("Couldn't inspect the page: {e}")),
        Err(_) => return Err("Couldn't inspect the page in time".into()),
    };
    let value = v.pointer("/result/value").cloned().unwrap_or(Value::Null);
    judge(expect, invert, &value).map(Some)
}

/// Turns the script's result into the check's verdict (pure, for tests).
pub fn judge(expect: &Expect, invert: bool, value: &Value) -> Result<String, String> {
    match expect {
        Expect::Nothing => Ok(String::new()),
        Expect::Keyword(k) => {
            let shown = util::truncate(k, 40);
            let found = value
                .get("i")
                .and_then(Value::as_i64)
                .is_some_and(|i| i >= 0);
            // The page text stays out of the message, which every role sees
            // and notifications carry; the screenshot is the place for it.
            let page_is_empty = value
                .get("head")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .is_empty();
            match (found, invert) {
                (true, false) => Ok(format!("keyword \"{shown}\" found")),
                (false, true) => Ok(format!("keyword \"{shown}\" absent, as expected")),
                (false, false) if page_is_empty => Err(format!(
                    "Keyword \"{shown}\" not found — the page has no text"
                )),
                (false, false) => Err(format!("Keyword \"{shown}\" not found")),
                (true, true) => Err(format!("Keyword \"{shown}\" found but should be absent")),
            }
        }
        Expect::Selector(s) => {
            let shown = util::truncate(s, 60);
            if let Some(err) = value.get("error").and_then(Value::as_str) {
                return Err(format!(
                    "Invalid selector {shown}: {}",
                    util::truncate(err, 120)
                ));
            }
            match (
                value.get("found").and_then(Value::as_bool).unwrap_or(false),
                invert,
            ) {
                (true, false) => Ok(format!("{shown} found")),
                (false, true) => Ok(format!("{shown} absent, as expected")),
                (false, false) => Err(format!("Selector {shown} not found")),
                (true, true) => Err(format!("Selector {shown} found but should be absent")),
            }
        }
    }
}
