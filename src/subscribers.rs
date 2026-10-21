//! Status page subscribers: sending confirmation mails and announcements.
//!
//! Visitors subscribe on the public page (email with double opt-in, or a
//! webhook). When an editor posts an incident, an update or a resolution, or
//! a maintenance window starts or ends on a page's monitors, [`announce`]
//! tells every confirmed subscriber of that page — one message per
//! subscriber, so nobody sees anybody else's address.
//!
//! Mail goes through an SMTP notification channel the editor picked for the
//! page. Webhook targets are chosen by strangers, so they only ever reach
//! public addresses (see [`resolve_public`]).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use lettre::Message;
use lettre::message::{Mailbox, MultiPart, header};
use serde_json::{Value, json};

use crate::api::AppState;
use crate::db::incidents as db_incidents;
use crate::db::status_pages::{self as db_pages, StatusPage};
use crate::db::subscribers::{self as db, Subscriber};
use crate::i18n::Lang;
use crate::notification::message::escape_html;

/// What happened on the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Incident,
    Update,
    Resolved,
    MaintenanceStart,
    MaintenanceEnd,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Incident => "incident",
            Kind::Update => "update",
            Kind::Resolved => "resolved",
            Kind::MaintenanceStart => "maintenance_start",
            Kind::MaintenanceEnd => "maintenance_end",
        }
    }

    fn label_key(self) -> &'static str {
        match self {
            Kind::Incident => "sub.kind_incident",
            Kind::Update => "sub.kind_update",
            Kind::Resolved => "sub.kind_resolved",
            Kind::MaintenanceStart => "sub.kind_maintenance_start",
            Kind::MaintenanceEnd => "sub.kind_maintenance_end",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Announcement {
    pub page_id: i64,
    pub kind: Kind,
    pub title: String,
    pub body: String,
    pub severity: String,
    pub incident_id: Option<i64>,
}

/// Tells the page's subscribers, in the background.
pub fn announce(state: &AppState, a: Announcement) {
    let state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = run(&state, a).await {
            tracing::warn!(error = %e, "subscriber announcement failed");
        }
    });
}

/// [`announce`] for an incident: the incident row supplies the text, and for
/// an update the newest timeline entry.
pub fn announce_incident(state: &AppState, page_id: i64, incident_id: i64, kind: Kind) {
    let state = state.clone();
    tokio::spawn(async move {
        let found = state
            .with_conn(move |c| {
                let Some(incident) = db_incidents::get(c, page_id, incident_id)? else {
                    return Ok(None);
                };
                let latest = match kind {
                    Kind::Update | Kind::Resolved => db_incidents::list_updates(c, incident_id)?
                        .pop()
                        .map(|u| u.body),
                    _ => None,
                };
                Ok(Some((incident, latest)))
            })
            .await;
        let Ok(Some((incident, latest))) = found else {
            return;
        };
        let body = latest
            .filter(|b| !b.is_empty())
            .unwrap_or(incident.body.clone());
        announce(
            &state,
            Announcement {
                page_id,
                kind,
                title: incident.title,
                body,
                severity: incident.severity,
                incident_id: Some(incident_id),
            },
        );
    });
}

/// Maintenance started or ended on `monitor_ids`: tell the pages showing them.
pub async fn announce_maintenance(state: &AppState, kind: Kind, title: &str, monitor_ids: &[i64]) {
    let ids = monitor_ids.to_vec();
    let Ok(pages) = state
        .with_conn(move |c| db::pages_with_monitors(c, &ids))
        .await
    else {
        return;
    };
    for page_id in pages {
        announce(
            state,
            Announcement {
                page_id,
                kind,
                title: title.to_string(),
                body: String::new(),
                severity: "info".into(),
                incident_id: None,
            },
        );
    }
}

struct Plan {
    page: StatusPage,
    subscribers: Vec<Subscriber>,
    mail: Option<Value>,
    webhooks: bool,
}

/// Loads everything a send needs; `None` when the page doesn't (or no longer)
/// offer subscriptions.
fn plan(conn: &rusqlite::Connection, page_id: i64) -> rusqlite::Result<Option<Plan>> {
    let Some(page) = db_pages::get(conn, page_id)? else {
        return Ok(None);
    };
    let settings = db::settings(conn, page_id)?;
    if !settings.enabled || !page.published {
        return Ok(None);
    }
    Ok(Some(Plan {
        subscribers: db::list_confirmed(conn, page_id)?,
        mail: mail_config(conn, settings.channel_id)?,
        webhooks: settings.webhooks,
        page,
    }))
}

/// The SMTP settings of the page's channel, if it is a working email channel.
fn mail_config(
    conn: &rusqlite::Connection,
    channel_id: Option<i64>,
) -> rusqlite::Result<Option<Value>> {
    let Some(id) = channel_id else {
        return Ok(None);
    };
    Ok(crate::db::notifications::get_channel(conn, id)?
        .filter(|c| c.active && c.kind == "email")
        .and_then(|c| serde_json::from_str(&c.config).ok()))
}

async fn run(state: &AppState, a: Announcement) -> Result<(), String> {
    let page_id = a.page_id;
    let Some(plan) = state
        .with_conn(move |c| plan(c, page_id))
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(());
    };
    let site_base = site_base_url(state).await;
    for sub in &plan.subscribers {
        let lang = Lang::from_code(&sub.lang).unwrap_or_else(crate::i18n::site_default);
        let base = if sub.base_url.is_empty() {
            site_base.clone()
        } else {
            sub.base_url.clone()
        };
        let sent = crate::i18n::scope(lang, deliver(&plan, sub, &base, &a)).await;
        if let Err(e) = &sent {
            tracing::warn!(page = plan.page.id, subscriber = sub.id, kind = %sub.kind, error = %e, "subscriber delivery failed");
        }
        let id = sub.id;
        let ok = sent.is_ok();
        let _ = state
            .with_conn(move |c| db::record_delivery(c, id, ok))
            .await;
    }
    Ok(())
}

async fn deliver(
    plan: &Plan,
    sub: &Subscriber,
    base: &str,
    a: &Announcement,
) -> Result<(), String> {
    let links = Links::new(base, &plan.page, &sub.token);
    match sub.kind.as_str() {
        "email" => {
            let config = plan.mail.as_ref().ok_or("no email channel")?;
            let message = announcement_mail(config, &plan.page, &sub.target, &links, a)?;
            crate::notification::email::deliver(config, message).await
        }
        "webhook" if plan.webhooks => {
            send_webhook(&sub.target, &announcement_json(&plan.page, &links, a)).await
        }
        _ => Ok(()),
    }
}

/// Mails the confirmation link. For a webhook the same information goes out
/// as JSON, so whoever runs the endpoint can open the link.
pub async fn send_confirmation(
    state: &AppState,
    page_id: i64,
    sub: &Subscriber,
    base: &str,
) -> Result<(), String> {
    let Some(plan) = state
        .with_conn(move |c| plan(c, page_id))
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(());
    };
    let links = Links::new(base, &plan.page, &sub.token);
    let lang = Lang::from_code(&sub.lang).unwrap_or_else(crate::i18n::site_default);
    crate::i18n::scope(lang, async {
        if sub.kind == "email" {
            let config = plan.mail.as_ref().ok_or("no email channel")?;
            let message = confirmation_mail(config, &plan.page, &sub.target, &links)?;
            crate::notification::email::deliver(config, message).await
        } else {
            let body = json!({
                "type": "confirm",
                "page": {"title": plan.page.title, "url": links.page},
                "confirm_url": links.confirm,
                "unsubscribe_url": links.unsubscribe,
            });
            send_webhook(&sub.target, &body).await
        }
    })
    .await
}

/// The base URL for links: the configured one, else what the visitor used.
pub async fn site_base_url(state: &AppState) -> String {
    state
        .settings()
        .await
        .ok()
        .and_then(|s| s.base_url)
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string()
}

struct Links {
    page: String,
    confirm: String,
    unsubscribe: String,
}

impl Links {
    fn new(base: &str, page: &StatusPage, token: &str) -> Self {
        let base = base.trim_end_matches('/');
        let page_url = format!("{base}/status/{}", page.slug);
        Self {
            confirm: format!("{page_url}/subscribe/confirm/{token}"),
            unsubscribe: format!("{page_url}/subscribe/unsubscribe/{token}"),
            page: page_url,
        }
    }
}

fn mailbox_from(config: &Value) -> Result<Mailbox, String> {
    config
        .get("from")
        .and_then(Value::as_str)
        .ok_or("the email channel has no From address")?
        .parse()
        .map_err(|e| format!("invalid from address: {e}"))
}

fn base_message(
    config: &Value,
    to: &str,
    subject: String,
    unsubscribe: Option<&str>,
) -> Result<lettre::message::MessageBuilder, String> {
    let to: Mailbox = to.parse().map_err(|e| format!("invalid address: {e}"))?;
    let mut b = Message::builder()
        .from(mailbox_from(config)?)
        .to(to)
        .subject(subject);
    if let Some(url) = unsubscribe {
        // Mail clients show an "unsubscribe" button for this header; the
        // link opens a confirmation page, so a scanner prefetching it
        // unsubscribes nobody.
        b = b.raw_header(header::HeaderValue::new(
            header::HeaderName::new_from_ascii_str("List-Unsubscribe"),
            format!("<{url}>"),
        ));
    }
    Ok(b)
}

fn html_shell(heading: &str, inner: &str, footer: &str) -> String {
    format!(
        "<!doctype html><html><body style=\"margin:0;padding:24px;background:#f5f5f7;font-family:-apple-system,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;color:#1d1d1f\">\
         <div style=\"max-width:560px;margin:0 auto;background:#fff;border-radius:14px;padding:28px\">\
         <h1 style=\"font-size:20px;margin:0 0 16px\">{heading}</h1>{inner}\
         <p style=\"font-size:12px;color:#6e6e73;margin:24px 0 0\">{footer}</p></div></body></html>"
    )
}

fn confirmation_mail(
    config: &Value,
    page: &StatusPage,
    to: &str,
    links: &Links,
) -> Result<Message, String> {
    let subject = crate::i18n::tf("sub.confirm_subject", &[("page", &page.title)]);
    let intro = crate::i18n::tf("sub.confirm_body", &[("page", &page.title)]);
    let ignore = crate::i18n::t("sub.confirm_ignore");
    let button = crate::i18n::t("sub.confirm_button");
    let text = format!("{intro}\n\n{}\n\n{ignore}\n", links.confirm);
    let html = html_shell(
        &escape_html(&subject),
        &format!(
            "<p style=\"font-size:15px;line-height:1.5\">{}</p>\
             <p><a href=\"{}\" style=\"display:inline-block;background:#007aff;color:#fff;text-decoration:none;padding:10px 18px;border-radius:10px;font-weight:600\">{}</a></p>",
            escape_html(&intro),
            escape_html(&links.confirm),
            escape_html(button),
        ),
        &escape_html(ignore),
    );
    base_message(config, to, subject, None)?
        .multipart(MultiPart::alternative_plain_html(text, html))
        .map_err(|e| e.to_string())
}

fn announcement_mail(
    config: &Value,
    page: &StatusPage,
    to: &str,
    links: &Links,
    a: &Announcement,
) -> Result<Message, String> {
    let label = crate::i18n::t(a.kind.label_key());
    let subject = format!("[{}] {label}: {}", page.title, a.title);
    let footer = crate::i18n::tf("sub.update_footer", &[("page", &page.title)]);
    let unsub = crate::i18n::t("sub.unsubscribe");
    let view = crate::i18n::t("sub.view_page");
    let mut text = format!("{label}: {}\n", a.title);
    if !a.body.trim().is_empty() {
        text.push_str(&format!("\n{}\n", a.body.trim()));
    }
    text.push_str(&format!(
        "\n{view}: {}\n\n{footer}\n{unsub}: {}\n",
        links.page, links.unsubscribe
    ));
    let body_html = if a.body.trim().is_empty() {
        String::new()
    } else {
        format!(
            "<p style=\"font-size:15px;line-height:1.5;white-space:pre-wrap\">{}</p>",
            escape_html(a.body.trim())
        )
    };
    let html = html_shell(
        &escape_html(&format!("{label}: {}", a.title)),
        &format!(
            "{body_html}<p><a href=\"{}\" style=\"color:#007aff\">{}</a></p>",
            escape_html(&links.page),
            escape_html(view)
        ),
        &format!(
            "{} · <a href=\"{}\" style=\"color:#6e6e73\">{}</a>",
            escape_html(&footer),
            escape_html(&links.unsubscribe),
            escape_html(unsub)
        ),
    );
    base_message(config, to, subject, Some(&links.unsubscribe))?
        .multipart(MultiPart::alternative_plain_html(text, html))
        .map_err(|e| e.to_string())
}

fn announcement_json(page: &StatusPage, links: &Links, a: &Announcement) -> Value {
    json!({
        "type": a.kind.as_str(),
        "page": {"title": page.title, "url": links.page},
        "title": a.title,
        "body": a.body,
        "severity": a.severity,
        "incident_id": a.incident_id,
        "unsubscribe_url": links.unsubscribe,
    })
}

// ---------------------------------------------------------------------------
// Webhook delivery (SSRF-safe)
// ---------------------------------------------------------------------------

/// Whether `ip` is a globally routable unicast address.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (64..128).contains(&o[1])) // CGNAT
                || (o[0] == 198 && (18..20).contains(&o[1])) // benchmarking
                || o[0] >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let seg = v6.segments();
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00 // unique local
                || (seg[0] & 0xffc0) == 0xfe80 // link local
                || (seg[0] == 0x2001 && seg[1] == 0x0db8)) // documentation
        }
    }
}

/// Parses a subscriber-supplied webhook URL (syntax only: http(s), a host,
/// no credentials).
pub fn parse_webhook_url(raw: &str) -> Option<url::Url> {
    let url = url::Url::parse(raw.trim()).ok()?;
    let ok = matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && raw.len() <= 2048;
    ok.then_some(url)
}

/// Resolves the URL's host and returns the addresses to connect to — only if
/// *every* one is public, so a name that also points inside can't be used to
/// reach internal services. The caller pins the connection to these.
pub async fn resolve_public(url: &url::Url) -> Result<Vec<SocketAddr>, String> {
    let host = url.host_str().ok_or("missing host")?;
    let port = url.port_or_known_default().ok_or("missing port")?;
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.trim_matches(['[', ']']), port))
        .await
        .map_err(|_| "could not resolve the host".to_string())?
        .collect();
    if addrs.is_empty() || addrs.iter().any(|a| !is_public(a.ip())) {
        return Err("the address is not a public one".into());
    }
    Ok(addrs)
}

async fn send_webhook(target: &str, body: &Value) -> Result<(), String> {
    let url = parse_webhook_url(target).ok_or("invalid webhook URL")?;
    let addrs = resolve_public(&url).await?;
    post_pinned(url, &addrs, body).await
}

/// POSTs `body`, connecting only to `addrs` (what was just checked) and
/// following no redirects, which could lead somewhere internal.
async fn post_pinned(url: url::Url, addrs: &[SocketAddr], body: &Value) -> Result<(), String> {
    let mut client = crate::tls_roots::http_client()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("VigilantVole/", env!("CARGO_PKG_VERSION")));
    if let Some(host) = url.host_str()
        && host.parse::<IpAddr>().is_err()
    {
        client = client.resolve_to_addrs(host, addrs);
    }
    let resp = client
        .build()
        .map_err(|e| e.to_string())?
        .post(url)
        .json(body)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                "timed out".to_string()
            } else {
                "request failed".to_string()
            }
        })?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("HTTP {}", resp.status()))
    }
}

// ---------------------------------------------------------------------------
// Sign-up throttling
// ---------------------------------------------------------------------------

static ATTEMPTS: LazyLock<Mutex<HashMap<String, (Instant, u32)>>> = LazyLock::new(Mutex::default);
const MAX_KEYS: usize = 20_000;

/// Counts one attempt for `key`; `false` once `limit` attempts happened
/// within `window`.
pub fn allow(key: &str, limit: u32, window: Duration) -> bool {
    let now = Instant::now();
    let Ok(mut map) = ATTEMPTS.lock() else {
        return true;
    };
    if map.len() >= MAX_KEYS {
        map.retain(|_, (start, _)| now.duration_since(*start) < window);
        if map.len() >= MAX_KEYS {
            map.clear();
        }
    }
    let entry = map.entry(key.to_string()).or_insert((now, 0));
    if now.duration_since(entry.0) >= window {
        *entry = (now, 0);
    }
    entry.1 += 1;
    entry.1 <= limit
}

#[cfg(test)]
mod tests;
