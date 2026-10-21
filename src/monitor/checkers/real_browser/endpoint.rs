//! Remote browser endpoints: validation, secret masking, `/json/version`
//! discovery and the WebSocket connection.
//!
//! Accepted forms:
//! - `ws(s)://host:port/devtools/browser/<id>`: Chrome's own browser endpoint;
//! - `http(s)://host:port`: discovered through `GET /json/version`
//!   (`webSocketDebuggerUrl`);
//! - `ws(s)://host[:port][/path]?token=…`: browserless and similar services.
//!
//! Chrome only serves DevTools to requests whose `Host` is an IP address or
//! `localhost`. When it refuses a host name, discovery and the WebSocket
//! handshake are retried with `Host: localhost:<port>`; the URL Chrome then
//! reports is pointed back at the configured host and port.

use std::time::Duration;

use serde_json::Value;
use tokio_tungstenite::Connector;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Error as WsError};
use url::Url;

use super::cdp::{self, Connection, EventFilter, Events};
use crate::monitor::checkers::{tls, util};
use crate::notification::SECRET_MASK;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Longest accepted endpoint URL.
pub const MAX_URL_LEN: usize = 2048;

/// Parses and checks a remote browser URL.
pub fn parse(raw: &str) -> Result<Url, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("Enter the browser's DevTools address".into());
    }
    if raw.chars().count() > MAX_URL_LEN {
        return Err(format!("Keep the address under {MAX_URL_LEN} characters"));
    }
    let url = Url::parse(raw).map_err(|_| {
        "Enter a full address like ws://chrome:9222 or http://chrome:9222".to_string()
    })?;
    if !matches!(url.scheme(), "ws" | "wss" | "http" | "https") {
        return Err(format!(
            "Use ws://, wss://, http:// or https:// (not {}://)",
            url.scheme()
        ));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("The address needs a host".into());
    }
    Ok(url)
}

fn sensitive(name: &str) -> bool {
    crate::config_io::is_sensitive_header(name)
}

/// The URL with its password and secret query values (`token`, `key`, …)
/// replaced by [`SECRET_MASK`], for pages, the API and exports. Text that
/// isn't a URL is returned as is.
pub fn masked(raw: &str) -> String {
    let Ok(url) = Url::parse(raw.trim()) else {
        return raw.trim().to_string();
    };
    let mut out = format!("{}://", url.scheme());
    if !url.username().is_empty() || url.password().is_some() {
        out.push_str(url.username());
        if url.password().is_some() {
            out.push(':');
            out.push_str(SECRET_MASK);
        }
        out.push('@');
    }
    if let Some(host) = url.host() {
        out.push_str(&host.to_string());
    }
    if let Some(port) = url.port() {
        out.push_str(&format!(":{port}"));
    }
    if url.path() != "/" || url.query().is_none() {
        out.push_str(url.path());
    }
    if let Some(query) = url.query() {
        out.push('?');
        let parts: Vec<String> = query
            .split('&')
            .map(|pair| {
                let key = pair.split('=').next().unwrap_or_default();
                let name: String = url::form_urlencoded::parse(key.as_bytes())
                    .map(|(k, _)| k.into_owned())
                    .next()
                    .unwrap_or_default();
                if pair.contains('=') && sensitive(&name) {
                    format!("{key}={SECRET_MASK}")
                } else {
                    pair.to_string()
                }
            })
            .collect();
        out.push_str(&parts.join("&"));
    }
    // Nothing secret: show it as it was entered.
    if out.contains(SECRET_MASK) {
        out
    } else {
        raw.trim().to_string()
    }
}

/// Whether the URL carries a secret ([`masked`] changes it).
pub fn has_secret(raw: &str) -> bool {
    masked(raw).contains(SECRET_MASK)
}

fn decoded(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={s}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

/// Why a masked URL couldn't be completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unmask {
    /// Nothing stored to fill it from, or it now points somewhere else.
    Reenter,
}

fn destination(url: &Url) -> (String, String, Option<u16>) {
    (
        url.scheme().to_string(),
        url.host_str().unwrap_or_default().to_ascii_lowercase(),
        url.port_or_known_default(),
    )
}

/// Fills the masked parts of a submitted URL from the stored one. A secret
/// is only kept while the URL still points at the same scheme, host and
/// port; otherwise it must be entered again (or an editor could send it to
/// a server of their own).
pub fn unmask(submitted: &str, stored: Option<&str>) -> Result<String, Unmask> {
    let submitted = submitted.trim();
    if !submitted.contains(SECRET_MASK) && !submitted.contains("%E2%80%A2") {
        return Ok(submitted.to_string());
    }
    let mut url = Url::parse(submitted).map_err(|_| Unmask::Reenter)?;
    let old = stored
        .and_then(|s| Url::parse(s.trim()).ok())
        .ok_or(Unmask::Reenter)?;
    if destination(&url) != destination(&old) {
        return Err(Unmask::Reenter);
    }
    if url.password().is_some_and(|p| decoded(p) == SECRET_MASK) {
        let pass = old.password().map(decoded).ok_or(Unmask::Reenter)?;
        url.set_password(Some(&pass))
            .map_err(|()| Unmask::Reenter)?;
    }
    if url.query().is_some() {
        let old_pairs: Vec<(String, String)> = old
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let mut pairs = Vec::new();
        for (k, v) in url.query_pairs() {
            let v = if v == SECRET_MASK {
                old_pairs
                    .iter()
                    .find(|(ok, _)| *ok == k)
                    .map(|(_, ov)| ov.clone())
                    .ok_or(Unmask::Reenter)?
            } else {
                v.into_owned()
            };
            pairs.push((k.into_owned(), v));
        }
        url.query_pairs_mut().clear().extend_pairs(pairs);
    }
    Ok(url.to_string())
}

/// Where to open the CDP WebSocket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub ws_url: Url,
    /// `Host` header to send instead of the URL's (see the module docs).
    pub host_header: Option<String>,
}

fn localhost_header(url: &Url) -> String {
    match url.port_or_known_default() {
        Some(port) => format!("localhost:{port}"),
        None => "localhost".into(),
    }
}

/// The browser's answer meant "use an IP address or localhost as Host".
fn host_refused(status: u16, body: &str) -> bool {
    status == 500 && body.contains("Host header")
}

/// `ws(s)://` URLs are used as they are; `http(s)://` ones are discovered
/// through `/json/version`.
pub async fn resolve(url: &Url, timeout: Duration) -> Result<Target, String> {
    match url.scheme() {
        "ws" | "wss" => Ok(Target {
            ws_url: url.clone(),
            host_header: None,
        }),
        _ => discover(url, timeout).await,
    }
}

async fn discover(url: &Url, timeout: Duration) -> Result<Target, String> {
    let mut version = url.clone();
    version.set_path("/json/version");
    version.set_fragment(None);
    let client = crate::tls_roots::http_client()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(crate::server::USER_AGENT)
        .build()
        .map_err(|e| e.to_string())?;
    let fetch = |host: Option<String>| {
        let mut req = client.get(version.clone());
        if let Some(h) = host {
            req = req.header(reqwest::header::HOST, h);
        }
        async move {
            let resp = req.send().await.map_err(|e| util::root_cause(&e))?;
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            Ok::<_, String>((status, body))
        }
    };
    let (mut status, mut body) = fetch(None).await?;
    let mut host_header = None;
    if host_refused(status, &body) {
        let h = localhost_header(url);
        (status, body) = fetch(Some(h.clone())).await?;
        host_header = Some(h);
    }
    if status != 200 {
        return Err(format!("{version} answered HTTP {status}"));
    }
    let info: Value =
        serde_json::from_str(&body).map_err(|_| format!("{version} didn't return JSON"))?;
    let reported = info
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{version} has no webSocketDebuggerUrl"))?;
    let reported = Url::parse(reported)
        .map_err(|_| "the browser reported an invalid webSocketDebuggerUrl".to_string())?;
    // Chrome reports the URL as it was reached (often localhost / 127.0.0.1):
    // keep its path, but use the configured host, port and TLS.
    let mut ws_url = url.clone();
    ws_url
        .set_scheme(if url.scheme() == "https" { "wss" } else { "ws" })
        .map_err(|()| "invalid URL".to_string())?;
    ws_url.set_path(reported.path());
    ws_url.set_query(reported.query().or(url.query()));
    ws_url.set_fragment(None);
    Ok(Target {
        ws_url,
        host_header,
    })
}

async fn open(target: &Url, host: Option<&str>) -> Result<Ws, WsError> {
    let mut request = target.as_str().into_client_request()?;
    request.headers_mut().insert(
        "user-agent",
        HeaderValue::from_static(crate::server::USER_AGENT),
    );
    if let Some(h) = host
        && let Ok(v) = HeaderValue::from_str(h)
    {
        request.headers_mut().insert("host", v);
    }
    let connector = if target.scheme() == "wss" {
        tls::verified_config().map(Connector::Rustls)
    } else {
        None
    };
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(cdp::MAX_MESSAGE_BYTES))
        .max_frame_size(Some(cdp::MAX_MESSAGE_BYTES));
    let (ws, _) =
        tokio_tungstenite::connect_async_tls_with_config(request, Some(config), true, connector)
            .await?;
    Ok(ws)
}

fn describe(err: &WsError) -> String {
    match err {
        WsError::Http(resp) => {
            let body = resp
                .body()
                .as_deref()
                .map(String::from_utf8_lossy)
                .unwrap_or_default();
            let body = util::snippet(&body, 120);
            let status = resp.status();
            let reason = status
                .canonical_reason()
                .map(|r| format!(" {r}"))
                .unwrap_or_default();
            if body.is_empty() {
                format!(
                    "the handshake was refused (HTTP {}{reason})",
                    status.as_u16()
                )
            } else {
                format!(
                    "the handshake was refused (HTTP {}{reason}): {body}",
                    status.as_u16()
                )
            }
        }
        other => util::root_cause(other),
    }
}

/// Opens the CDP WebSocket and starts the client.
pub async fn connect(target: &Target, filter: EventFilter) -> Result<(Connection, Events), String> {
    let ws = match open(&target.ws_url, target.host_header.as_deref()).await {
        Ok(ws) => ws,
        Err(WsError::Http(resp))
            if target.host_header.is_none()
                && host_refused(
                    resp.status().as_u16(),
                    &resp
                        .body()
                        .as_deref()
                        .map(String::from_utf8_lossy)
                        .unwrap_or_default(),
                ) =>
        {
            open(&target.ws_url, Some(&localhost_header(&target.ws_url)))
                .await
                .map_err(|e| describe(&e))?
        }
        Err(e) => return Err(describe(&e)),
    };
    Ok(Connection::start(ws, filter))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_validated() {
        for ok in [
            "ws://chrome:9222/devtools/browser/abc",
            "wss://chrome.example.com?token=x",
            "http://10.0.0.5:9222",
            "https://b.example",
        ] {
            assert!(parse(ok).is_ok(), "{ok}");
        }
        for bad in ["", "chrome:9222", "ftp://chrome", "http://", "not a url"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn tokens_and_passwords_are_masked() {
        assert_eq!(
            masked("wss://chrome.example.com?token=abc123&launch=x"),
            format!("wss://chrome.example.com?token={SECRET_MASK}&launch=x")
        );
        assert_eq!(
            masked("ws://user:pw@chrome:3000/chromium?apiKey=k"),
            format!("ws://user:{SECRET_MASK}@chrome:3000/chromium?apiKey={SECRET_MASK}")
        );
        assert_eq!(
            masked("ws://chrome:9222/devtools/browser/abc"),
            "ws://chrome:9222/devtools/browser/abc"
        );
        assert_eq!(masked(" http://[::1]:9222 "), "http://[::1]:9222");
        assert!(
            has_secret("wss://b.io?token=x")
                && has_secret("ws://u:p@b.io")
                && !has_secret("ws://b.io/devtools/browser/1")
        );
    }

    #[test]
    fn masked_secrets_are_kept_only_for_the_same_destination() {
        let stored = "wss://chrome.example.com?token=s3cr%26t&launch=1";
        let shown = masked(stored);
        let back = unmask(&shown, Some(stored)).unwrap();
        let pairs: Vec<(String, String)> = Url::parse(&back)
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("token".to_string(), "s3cr&t".to_string()),
                ("launch".into(), "1".into())
            ]
        );
        // Same host, other options: still fine.
        let edited = shown.replace("launch=1", "launch=2");
        assert!(
            unmask(&edited, Some(stored))
                .unwrap()
                .contains("token=s3cr%26t")
        );
        // Another host, or nothing stored: re-enter.
        assert_eq!(
            unmask(
                &shown.replace("chrome.example.com", "evil.example"),
                Some(stored)
            ),
            Err(Unmask::Reenter)
        );
        assert_eq!(unmask(&shown, None), Err(Unmask::Reenter));
        // Passwords too.
        let with_pw = "ws://bot:hunter2@chrome:3000";
        assert_eq!(
            unmask(&masked(with_pw), Some(with_pw)).unwrap(),
            "ws://bot:hunter2@chrome:3000/"
        );
        // Nothing masked: taken as typed.
        assert_eq!(unmask(" ws://new:1 ", Some(stored)).unwrap(), "ws://new:1");
    }
}
