//! Real-browser checks against a scripted fake browser: a 127.0.0.1 server
//! that answers `/json/version` and speaks just enough CDP over WebSocket,
//! logging every command it receives.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

use super::*;
use crate::db::models::{HeartbeatStatus, MonitorType};
use crate::monitor::checkers::test_support;

const JPEG: &[u8] = b"\xff\xd8\xff\xe0fake-jpeg";

/// How the fake browser behaves.
#[derive(Clone)]
struct Script {
    status: u16,
    load: bool,
    idle: bool,
    /// Never answer `Page.navigate`.
    hang_navigate: bool,
    nav_error: Option<&'static str>,
    exception: Option<&'static str>,
    /// What `Runtime.evaluate` returns as the value.
    eval: Value,
    /// Refuse requests whose Host isn't localhost (like Chrome does for host names).
    strict_host: bool,
}

impl Default for Script {
    fn default() -> Self {
        Script {
            status: 200,
            load: true,
            idle: true,
            hang_navigate: false,
            nav_error: None,
            exception: None,
            eval: json!({"found": true}),
            strict_host: false,
        }
    }
}

type Log = Arc<Mutex<Vec<String>>>;

struct Fake {
    addr: SocketAddr,
    log: Log,
}

impl Fake {
    fn ws(&self) -> String {
        format!("ws://{}/devtools/browser/fake", self.addr)
    }

    fn http(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn methods(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    async fn wait_for(&self, method: &str) -> bool {
        for _ in 0..100 {
            if self.methods().iter().any(|m| m == method) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }
}

async fn fake_browser(script: Script) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let server_log = log.clone();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            tokio::spawn(serve(tcp, script.clone(), server_log.clone(), addr));
        }
    });
    Fake { addr, log }
}

async fn request_head(tcp: &TcpStream) -> String {
    let mut buf = vec![0u8; 4096];
    loop {
        let n = tcp.peek(&mut buf).await.unwrap_or(0);
        let text = String::from_utf8_lossy(&buf[..n]).to_string();
        if text.contains("\r\n\r\n") || n == 0 || n == buf.len() {
            return text;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn host_ok(script: &Script, host: &str) -> bool {
    !script.strict_host || host.starts_with("localhost")
}

async fn serve(mut tcp: TcpStream, script: Script, log: Log, addr: SocketAddr) {
    let head = request_head(&tcp).await;
    if !head.to_ascii_lowercase().contains("upgrade: websocket") {
        let mut buf = vec![0u8; head.len()];
        let _ = tcp.read_exact(&mut buf).await;
        let host = head
            .lines()
            .find_map(|l| {
                l.strip_prefix("host: ")
                    .or_else(|| l.strip_prefix("Host: "))
            })
            .unwrap_or_default();
        log.lock().unwrap().push(format!("GET host={host}"));
        let (status, body) = if !host_ok(&script, host) {
            (
                "500 Internal Server Error",
                "Host header is specified and is not an IP address or localhost.".to_string(),
            )
        } else if head.starts_with("GET /json/version") {
            // Chrome reports the address it was reached at; use a wrong one
            // to check that the configured host and port win.
            ("200 OK", json!({"Browser": "HeadlessChrome/131.0", "webSocketDebuggerUrl": "ws://localhost:1/devtools/browser/abc-123"}).to_string())
        } else {
            ("404 Not Found", String::new())
        };
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = tcp.write_all(reply.as_bytes()).await;
        let _ = tcp.shutdown().await;
        return;
    }
    // Chrome refuses WebSocket handshakes whose Host is a name, too.
    let host = head
        .lines()
        .find_map(|l| {
            l.strip_prefix("host: ")
                .or_else(|| l.strip_prefix("Host: "))
        })
        .unwrap_or_default();
    if !host_ok(&script, host) {
        let mut buf = vec![0u8; head.len()];
        let _ = tcp.read_exact(&mut buf).await;
        let body = "Host header is specified and is not an IP address or localhost.";
        let reply = format!(
            "HTTP/1.1 500 Internal Server Error
Content-Length: {}
Connection: close

{body}",
            body.len()
        );
        let _ = tcp.write_all(reply.as_bytes()).await;
        let _ = tcp.shutdown().await;
        return;
    }
    let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else {
        return;
    };
    log.lock().unwrap().push(format!("WS path={}", addr));
    while let Some(Ok(msg)) = ws.next().await {
        let Message::Text(text) = msg else { continue };
        let cmd: Value = serde_json::from_str(&text).unwrap();
        let id = cmd["id"].clone();
        let method = cmd["method"].as_str().unwrap_or_default().to_string();
        log.lock().unwrap().push(method.clone());
        let event = |method: &str, params: Value| {
            json!({"method": method, "params": params, "sessionId": "S1"}).to_string()
        };
        let mut out: Vec<String> = Vec::new();
        let result = match method.as_str() {
            "Browser.getVersion" => {
                json!({"product": "HeadlessChrome/131.0.6778.85", "protocolVersion": "1.3", "userAgent": "Mozilla/5.0 HeadlessChrome"})
            }
            "Target.createBrowserContext" => json!({"browserContextId": "CTX"}),
            "Target.createTarget" => {
                assert_eq!(
                    cmd["params"]["browserContextId"], "CTX",
                    "pages open in the fresh context"
                );
                json!({"targetId": "T1"})
            }
            "Target.attachToTarget" => {
                assert_eq!(cmd["params"]["flatten"], true);
                json!({"sessionId": "S1"})
            }
            "Page.setLifecycleEventsEnabled" => {
                // Replayed events of the blank page must not count as the load.
                out.push(event(
                    "Page.lifecycleEvent",
                    json!({"frameId": "T1", "loaderId": "BLANK", "name": "init", "timestamp": 1.0}),
                ));
                out.push(event(
                    "Page.lifecycleEvent",
                    json!({"frameId": "T1", "loaderId": "BLANK", "name": "load", "timestamp": 1.0}),
                ));
                json!({})
            }
            "Page.navigate" => {
                if script.hang_navigate {
                    continue;
                }
                if let Some(err) = script.nav_error {
                    json!({"frameId": "T1", "loaderId": "L1", "errorText": err})
                } else {
                    let doc = |kind: &str, params: Value| {
                        let mut p = params;
                        p["type"] = json!(kind);
                        p
                    };
                    out.push(event(
                        "Network.requestWillBeSent",
                        doc(
                            "Document",
                            json!({"frameId": "T1", "loaderId": "L1", "timestamp": 100.0}),
                        ),
                    ));
                    // A sub-frame's failing document is not the page's status.
                    out.push(event("Network.responseReceived", doc("Document", json!({"frameId": "F2", "loaderId": "L9", "response": {"status": 500, "statusText": ""}}))));
                    out.push(event(
                        "Network.responseReceived",
                        doc("Document", json!({"frameId": "T1", "loaderId": "L1", "response": {"status": script.status, "statusText": ""}})),
                    ));
                    out.push(event("Page.lifecycleEvent", json!({"frameId": "T1", "loaderId": "L1", "name": "init", "timestamp": 100.0})));
                    if let Some(ex) = script.exception {
                        out.push(event("Runtime.exceptionThrown", json!({"exceptionDetails": {"text": "Uncaught", "exception": {"description": format!("{ex}\n    at app.js:1:1")}}})));
                    }
                    if script.load {
                        out.push(event("Page.lifecycleEvent", json!({"frameId": "T1", "loaderId": "L1", "name": "load", "timestamp": 101.84})));
                    }
                    if script.idle {
                        out.push(event("Page.lifecycleEvent", json!({"frameId": "T1", "loaderId": "L1", "name": "networkIdle", "timestamp": 102.5})));
                    }
                    // The answer comes first, the events after it.
                    let reply = json!({"id": id, "result": {"frameId": "T1", "loaderId": "L1"}, "sessionId": "S1"}).to_string();
                    out.insert(0, reply);
                    for frame in out {
                        let _ = ws.send(Message::text(frame)).await;
                    }
                    continue;
                }
            }
            "Runtime.evaluate" => json!({"result": {"type": "object", "value": script.eval}}),
            "Page.captureScreenshot" => {
                json!({"data": base64::engine::general_purpose::STANDARD.encode(JPEG)})
            }
            "Target.closeTarget" => json!({"success": true}),
            _ => json!({}),
        };
        for frame in out {
            let _ = ws.send(Message::text(frame)).await;
        }
        let mut reply = json!({"id": id, "result": result});
        if let Some(s) = cmd.get("sessionId") {
            reply["sessionId"] = s.clone();
        }
        let _ = ws.send(Message::text(reply.to_string())).await;
    }
}

fn options(url: &str) -> Options {
    let m = test_support::monitor(MonitorType::RealBrowser, url);
    Options::from_monitor(&m).unwrap()
}

#[tokio::test]
async fn a_loaded_page_is_up_with_status_and_load_time() {
    let fake = fake_browser(Script::default()).await;
    let r = run(
        &fake.ws(),
        &options("https://shop.example.com/"),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(r.outcome.status, HeartbeatStatus::Up, "{:?}", r.outcome);
    assert_eq!(
        r.outcome.message.as_deref(),
        Some("HTTP 200 · loaded in 1.84 s")
    );
    assert_eq!(
        r.outcome.response_time_ms,
        Some(1840),
        "document request → load, browser clock"
    );
    assert!(r.screenshot.is_none(), "no screenshot for a passing check");
    let methods = fake.methods();
    for m in [
        "Target.createBrowserContext",
        "Page.enable",
        "Network.enable",
        "Runtime.enable",
        "Emulation.setDeviceMetricsOverride",
        "Page.navigate",
    ] {
        assert!(
            methods.iter().any(|x| x == m),
            "{m} missing from {methods:?}"
        );
    }
    assert!(
        !methods.iter().any(|x| x == "Runtime.evaluate"),
        "nothing to look for"
    );
    // The page and its context are closed afterwards.
    assert!(
        fake.wait_for("Target.disposeBrowserContext").await,
        "{:?}",
        fake.methods()
    );
    assert!(fake.methods().iter().any(|m| m == "Target.closeTarget"));
}

#[tokio::test]
async fn a_failing_main_document_is_down_with_a_screenshot() {
    let fake = fake_browser(Script {
        status: 503,
        ..Script::default()
    })
    .await;
    let r = run(
        &fake.ws(),
        &options("https://shop.example.com/"),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(r.outcome.status, HeartbeatStatus::Down);
    assert_eq!(
        r.outcome.message.as_deref(),
        Some("HTTP 503 Service Unavailable")
    );
    assert_eq!(r.screenshot.as_deref(), Some(JPEG));
    assert!(fake.wait_for("Target.disposeBrowserContext").await);
}

#[tokio::test]
async fn selectors_and_keywords_are_checked_in_the_page() {
    let fake = fake_browser(Script {
        eval: json!({"found": false}),
        ..Script::default()
    })
    .await;
    let mut opts = options("https://shop.example.com/");
    opts.expect = Expect::Selector("#app".into());
    let r = run(&fake.ws(), &opts, Duration::from_secs(5)).await;
    assert_eq!(
        r.outcome.message.as_deref(),
        Some("Selector #app not found")
    );
    assert!(r.screenshot.is_some());

    let fake = fake_browser(Script {
        eval: json!({"i": 12, "head": "Welcome", "around": "x"}),
        ..Script::default()
    })
    .await;
    let mut opts = options("https://shop.example.com/");
    opts.expect = Expect::Keyword("Add to cart".into());
    let r = run(&fake.ws(), &opts, Duration::from_secs(5)).await;
    assert_eq!(r.outcome.status, HeartbeatStatus::Up);
    assert_eq!(
        r.outcome.message.as_deref(),
        Some("HTTP 200 · loaded in 1.84 s · keyword \"Add to cart\" found")
    );
}

#[tokio::test]
async fn javascript_exceptions_fail_the_check_when_asked() {
    let script = Script {
        exception: Some("TypeError: cart is undefined"),
        ..Script::default()
    };
    let fake = fake_browser(script.clone()).await;
    let mut opts = options("https://shop.example.com/");
    let run1 = run(&fake.ws(), &opts, Duration::from_secs(5)).await;
    assert_eq!(
        run1.outcome.status,
        HeartbeatStatus::Up,
        "ignored unless enabled"
    );
    opts.js_errors = true;
    let run2 = run(&fake.ws(), &opts, Duration::from_secs(5)).await;
    assert_eq!(run2.outcome.status, HeartbeatStatus::Down);
    assert_eq!(
        run2.outcome.message.as_deref(),
        Some("JavaScript error: TypeError: cart is undefined")
    );
}

#[tokio::test]
async fn navigation_errors_are_reported() {
    let fake = fake_browser(Script {
        nav_error: Some("net::ERR_NAME_NOT_RESOLVED"),
        ..Script::default()
    })
    .await;
    let r = run(
        &fake.ws(),
        &options("https://nope.invalid/"),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        r.outcome.message.as_deref(),
        Some("Navigation failed: net::ERR_NAME_NOT_RESOLVED")
    );
    assert!(
        r.screenshot.is_some(),
        "the browser's error page is captured"
    );
}

#[tokio::test]
async fn a_page_that_never_loads_times_out_with_a_screenshot_and_cleanup() {
    let fake = fake_browser(Script {
        load: false,
        idle: false,
        ..Script::default()
    })
    .await;
    let started = std::time::Instant::now();
    let r = run(
        &fake.ws(),
        &options("https://slow.example.com/"),
        Duration::from_secs(2),
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "finishes inside the monitor timeout"
    );
    assert_eq!(r.outcome.status, HeartbeatStatus::Down);
    assert!(
        r.outcome
            .message
            .as_deref()
            .unwrap()
            .starts_with("Page didn't finish loading within"),
        "{:?}",
        r.outcome
    );
    assert_eq!(r.screenshot.as_deref(), Some(JPEG));
    assert!(fake.wait_for("Target.disposeBrowserContext").await);

    // Waiting for network idle: loaded but never idle.
    let fake = fake_browser(Script {
        idle: false,
        ..Script::default()
    })
    .await;
    let mut opts = options("https://busy.example.com/");
    opts.wait = Wait::NetworkIdle;
    let r = run(&fake.ws(), &opts, Duration::from_secs(2)).await;
    let msg = r.outcome.message.unwrap();
    assert!(
        msg.starts_with("Network didn't go idle within") && msg.ends_with("(loaded in 1.84 s)"),
        "{msg}"
    );
}

#[tokio::test]
async fn an_aborted_check_still_closes_its_page() {
    let fake = fake_browser(Script {
        hang_navigate: true,
        ..Script::default()
    })
    .await;
    let endpoint = fake.ws();
    let task = tokio::spawn(async move {
        run(
            &endpoint,
            &options("https://hang.example.com/"),
            Duration::from_secs(30),
        )
        .await
    });
    assert!(fake.wait_for("Page.navigate").await);
    // What `run_check` does when the monitor's timeout passes.
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(
        fake.wait_for("Target.closeTarget").await,
        "{:?}",
        fake.methods()
    );
    assert!(
        fake.wait_for("Target.disposeBrowserContext").await,
        "{:?}",
        fake.methods()
    );
}

#[tokio::test]
async fn http_endpoints_are_discovered_through_json_version() {
    let fake = fake_browser(Script::default()).await;
    let r = run(
        &fake.http(),
        &options("https://shop.example.com/"),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(r.outcome.status, HeartbeatStatus::Up, "{:?}", r.outcome);
    let methods = fake.methods();
    assert!(methods[0].starts_with("GET host=127.0.0.1"), "{methods:?}");
    // The reported ws://localhost:1/… was pointed back at the configured host.
    assert!(
        methods.iter().any(|m| m.starts_with("WS path=")),
        "{methods:?}"
    );

    // Chrome refuses host names: retried with Host: localhost.
    let target = endpoint::resolve(
        &url::Url::parse(&fake.http()).unwrap(),
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert_eq!(
        target.ws_url.as_str(),
        format!("ws://{}/devtools/browser/abc-123", fake.addr)
    );
    let strict = fake_browser(Script {
        strict_host: true,
        ..Script::default()
    })
    .await;
    let target = endpoint::resolve(
        &url::Url::parse(&strict.http()).unwrap(),
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert_eq!(
        target.host_header.as_deref(),
        Some(format!("localhost:{}", strict.addr.port()).as_str())
    );
    let version = test_connection(&strict.http(), Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(version.product, "HeadlessChrome/131.0.6778.85");
    // A ws:// address to such a browser is retried the same way.
    let version = test_connection(&strict.ws(), Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(version.protocol, "1.3");
}

#[tokio::test]
async fn unreachable_browsers_are_reported() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let r = run(
        &format!("ws://{addr}/devtools/browser/x"),
        &options("https://x.example/"),
        Duration::from_secs(3),
    )
    .await;
    assert!(
        r.outcome
            .message
            .as_deref()
            .unwrap()
            .starts_with("Remote browser unreachable:"),
        "{:?}",
        r.outcome
    );
    assert!(
        test_connection(&format!("http://{addr}"), Duration::from_secs(2))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn one_browser_runs_a_bounded_number_of_checks_at_once() {
    let fake = fake_browser(Script {
        hang_navigate: true,
        ..Script::default()
    })
    .await;
    let mut tasks = Vec::new();
    for _ in 0..PER_BROWSER_CHECKS {
        let endpoint = fake.ws();
        tasks.push(tokio::spawn(async move {
            run(
                &endpoint,
                &options("https://hang.example.com/"),
                Duration::from_secs(4),
            )
            .await
        }));
    }
    while fake
        .methods()
        .iter()
        .filter(|m| *m == "Page.navigate")
        .count()
        < PER_BROWSER_CHECKS
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // One more, with a shorter timeout than the running ones take.
    let extra = run(
        &fake.ws(),
        &options("https://hang.example.com/"),
        Duration::from_secs(1),
    )
    .await;
    assert!(
        extra
            .outcome
            .message
            .as_deref()
            .unwrap()
            .starts_with("Remote browser busy"),
        "{:?}",
        extra.outcome
    );
    for t in tasks {
        let out = t.await.unwrap().outcome;
        assert!(
            out.message
                .as_deref()
                .unwrap()
                .starts_with("No response from the page within"),
            "{out:?}"
        );
    }
    let opened = fake
        .methods()
        .iter()
        .filter(|m| *m == "Target.createBrowserContext")
        .count();
    assert_eq!(opened, PER_BROWSER_CHECKS);
}

#[test]
fn verdicts_cover_keywords_selectors_and_inversion() {
    let kw = Expect::Keyword("OK".into());
    assert!(judge(&kw, false, &json!({"i": 0})).is_ok());
    assert_eq!(
        judge(&kw, false, &json!({"i": -1, "head": "Error   page\nhere"})).unwrap_err(),
        "Keyword \"OK\" not found"
    );
    assert_eq!(
        judge(&kw, false, &json!({"i": -1, "head": ""})).unwrap_err(),
        "Keyword \"OK\" not found — the page has no text"
    );
    assert!(judge(&kw, true, &json!({"i": -1})).is_ok());
    assert!(
        judge(&kw, true, &json!({"i": 3, "around": "all OK"}))
            .unwrap_err()
            .contains("should be absent")
    );
    let sel = Expect::Selector("div[data-x=\"1\"]".into());
    assert!(judge(&sel, false, &json!({"found": true})).is_ok());
    assert_eq!(
        judge(&sel, true, &json!({"found": true})).unwrap_err(),
        "Selector div[data-x=\"1\"] found but should be absent"
    );
    assert!(
        judge(
            &sel,
            false,
            &json!({"error": "'##' is not a valid selector"})
        )
        .unwrap_err()
        .starts_with("Invalid selector")
    );
}

#[test]
fn scripts_escape_what_they_look_for() {
    let tricky = "a\"b'c\\d</script>\n";
    let script = keyword_script(tricky);
    assert!(
        script.contains(&serde_json::to_string(tricky).unwrap()),
        "{script}"
    );
    assert!(selector_script("a[x='\"']").contains(r#""a[x='\"']""#));
}

#[test]
fn options_come_from_the_monitor() {
    let mut m = test_support::monitor(MonitorType::RealBrowser, "https://shop.example.com/");
    m.http_headers = Some(r#"{"X-Probe":"1","Authorization":"Bearer old"}"#.into());
    m.auth_method = "basic".into();
    m.basic_auth_user = Some("bot".into());
    m.basic_auth_pass = Some("pw".into());
    m.browser_selector = Some("#app".into());
    m.keyword = Some("ignored when a selector is set".into());
    m.browser_wait = "network_idle".into();
    m.browser_viewport = "mobile".into();
    let o = Options::from_monitor(&m).unwrap();
    assert_eq!(o.expect, Expect::Selector("#app".into()));
    assert_eq!((o.wait, o.viewport), (Wait::NetworkIdle, Viewport::Mobile));
    let auth: Vec<&(String, String)> = o
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .collect();
    assert_eq!(auth.len(), 1);
    assert_eq!(
        auth[0].1,
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("bot:pw")
        )
    );
    assert!(o.headers.iter().any(|(k, v)| k == "x-probe" && v == "1"));
    m.target = "file:///etc/passwd".into();
    assert!(Options::from_monitor(&m).is_err(), "only web pages");
}

#[tokio::test]
async fn the_server_check_reads_the_remote_browser_and_stores_the_screenshot() {
    let dir = std::env::temp_dir().join(format!(
        "vv-real-browser-{}-{}",
        std::process::id(),
        crate::db::agents::generate_token().hash
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let pool = crate::db::build_pool(&dir.join("test.db"));
    crate::db::run_migrations(&pool, &dir.join("backups")).await;
    install(pool.clone());
    let pool = DB.get().unwrap().clone();

    let mut m = test_support::monitor(MonitorType::RealBrowser, "https://shop.example.com/");
    let none = crate::db::interact(&pool, |c| {
        c.execute("DELETE FROM remote_browsers", [])?;
        Ok(())
    });
    none.await.unwrap();
    assert_eq!(check(&m).await.message.as_deref(), Some(NO_BROWSER));

    let fake = fake_browser(Script {
        status: 500,
        ..Script::default()
    })
    .await;
    let url = fake.ws();
    let new = crate::db::models::NewMonitor {
        name: "Shop".into(),
        monitor_type: MonitorType::RealBrowser,
        target: m.target.clone(),
        ..Default::default()
    };
    let id = crate::db::interact(&pool, move |c| {
        db_rb::insert(c, "Fake", &url)?;
        crate::db::monitors::insert_monitor(c, &new)
    })
    .await
    .unwrap();
    m.id = id;
    let out = check(&m).await;
    assert_eq!(
        out.message.as_deref(),
        Some("HTTP 500 Internal Server Error")
    );
    let mut stored = None;
    for _ in 0..100 {
        stored = crate::db::interact(&pool, move |c| db_rb::get_screenshot(c, id))
            .await
            .unwrap();
        if stored.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let shot = stored.expect("screenshot stored");
    assert_eq!(
        (shot.image.as_slice(), shot.message.as_deref()),
        (JPEG, Some("HTTP 500 Internal Server Error"))
    );
}

/// Against a real headless Chromium, e.g.
/// `docker run -d -p 127.0.0.1:9222:9222 chromedp/headless-shell:156.0.8078.36`, then
/// `VV_TEST_CDP=http://127.0.0.1:9222 cargo test --lib real_chromium -- --ignored --nocapture`.
/// Pages served by this test are reached from the container through
/// `VV_TEST_PAGE_HOST` (default `host.docker.internal`).
#[tokio::test]
#[ignore = "needs a headless Chromium (see the doc comment)"]
async fn real_chromium() {
    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::Html;
    use axum::routing::get;

    let cdp = std::env::var("VV_TEST_CDP").unwrap_or_else(|_| "http://127.0.0.1:9222".into());
    let page_host =
        std::env::var("VV_TEST_PAGE_HOST").unwrap_or_else(|_| "host.docker.internal".into());
    let version = test_connection(&cdp, Duration::from_secs(10))
        .await
        .expect("browser reachable");
    println!(
        "browser: {} (protocol {})",
        version.product, version.protocol
    );

    let app = Router::new()
        .route("/ok", get(|| async { Html("<html><body><h1>Shop</h1><div id=\"app\" data-ready>Add to cart</div></body></html>") }))
        .route("/late", get(|| async { Html("<html><body><script>setTimeout(() => { document.body.innerHTML += '<p id=\"late\">Loaded later</p>' }, 300)</script></body></html>") }))
        .route("/error", get(|| async { (StatusCode::SERVICE_UNAVAILABLE, Html("<h1>Down for maintenance</h1>")) }))
        .route("/js", get(|| async { Html("<html><body>Hi<script>window.cart.total()</script></body></html>") }))
        .route("/hang", get(|| async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            "never"
        }));
    let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let page = |path: &str| format!("http://{page_host}:{port}{path}");
    let report = |name: &str, r: &Run| {
        println!(
            "{name:<28} {:?} {:>6?} ms  {}  screenshot={}",
            r.outcome.status,
            r.outcome.response_time_ms,
            r.outcome.message.as_deref().unwrap_or(""),
            r.screenshot.as_ref().map_or(0, Vec::len)
        )
    };

    let mut opts = options("https://example.com/");
    opts.expect = Expect::Keyword("documentation examples".into());
    let r = run(&cdp, &opts, Duration::from_secs(20)).await;
    report("example.com keyword", &r);
    assert_eq!(r.outcome.status, HeartbeatStatus::Up, "{:?}", r.outcome);

    let mut opts = options(&page("/ok"));
    opts.expect = Expect::Selector("#app[data-ready]".into());
    let r = run(&cdp, &opts, Duration::from_secs(15)).await;
    report("local selector", &r);
    assert_eq!(r.outcome.status, HeartbeatStatus::Up, "{:?}", r.outcome);
    assert!(
        r.outcome
            .message
            .as_deref()
            .unwrap()
            .starts_with("HTTP 200 · loaded in")
    );

    opts.expect = Expect::Selector("#checkout".into());
    let r = run(&cdp, &opts, Duration::from_secs(15)).await;
    report("local missing selector", &r);
    assert_eq!(
        r.outcome.message.as_deref(),
        Some("Selector #checkout not found")
    );
    let jpeg = r.screenshot.expect("a screenshot");
    assert!(jpeg.starts_with(&[0xff, 0xd8]), "a JPEG");

    let mut opts = options(&page("/late"));
    opts.wait = Wait::NetworkIdle;
    opts.expect = Expect::Keyword("Loaded later".into());
    let r = run(&cdp, &opts, Duration::from_secs(15)).await;
    report("network idle + keyword", &r);
    assert_eq!(r.outcome.status, HeartbeatStatus::Up, "{:?}", r.outcome);

    let mut opts = options(&page("/ok"));
    opts.viewport = Viewport::Mobile;
    let r = run(&cdp, &opts, Duration::from_secs(15)).await;
    report("mobile viewport", &r);
    assert_eq!(r.outcome.status, HeartbeatStatus::Up);

    let r = run(&cdp, &options(&page("/error")), Duration::from_secs(15)).await;
    report("HTTP 503", &r);
    assert_eq!(
        r.outcome.message.as_deref(),
        Some("HTTP 503 Service Unavailable")
    );
    assert!(r.screenshot.is_some());

    let mut opts = options(&page("/js"));
    opts.js_errors = true;
    let r = run(&cdp, &opts, Duration::from_secs(15)).await;
    report("JavaScript error", &r);
    assert!(
        r.outcome
            .message
            .as_deref()
            .unwrap()
            .starts_with("JavaScript error: TypeError"),
        "{:?}",
        r.outcome
    );

    let r = run(&cdp, &options(&page("/hang")), Duration::from_secs(4)).await;
    report("hanging page", &r);
    assert_eq!(r.outcome.status, HeartbeatStatus::Down);

    let r = run(
        &cdp,
        &options("https://does-not-exist.invalid/"),
        Duration::from_secs(10),
    )
    .await;
    report("DNS failure", &r);
    assert!(
        r.outcome
            .message
            .as_deref()
            .unwrap()
            .starts_with("Navigation failed: net::ERR_NAME_NOT_RESOLVED"),
        "{:?}",
        r.outcome
    );
}
