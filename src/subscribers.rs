//! Status page subscribers: sending confirmation mails and announcements.
//!
//! Visitors subscribe on the public page: by email with double opt-in, or by
//! webhook, which an editor approves in the Subscribers tab (nothing is sent
//! to a webhook before that). When an editor posts an incident, an update or
//! a resolution, or a maintenance window starts or ends on a page's
//! monitors, [`announce`] tells every confirmed subscriber of that page — one
//! message per subscriber, so nobody sees anybody else's address.
//!
//! Links in messages always point at the configured public URL (Settings →
//! General), or else the page's custom domain; without either a page offers
//! no subscriptions ([`link_base`]). They are never built from a request's
//! `Host`, which whoever signs up controls.
//!
//! Mail goes through an SMTP notification channel the editor picked for the
//! page. Webhook targets are chosen by strangers, so they only ever reach
//! public addresses (see [`resolve_public`]).

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum_extra::extract::cookie::Key;
use futures_util::StreamExt as _;
use hmac::{Hmac, KeyInit as _, Mac as _};
use lettre::Message;
use lettre::message::{Mailbox, MultiPart, header};
use serde_json::{Value, json};
use sha2::Sha256;

use crate::api::AppState;
use crate::db::incidents as db_incidents;
use crate::db::status_pages::{self as db_pages, StatusPage};
use crate::db::subscribers::{self as db, Subscriber};
use crate::i18n::Lang;
use crate::notification::message::escape_html;

/// Start of a subscriber link token (`vvs_` + 64 hex digits). API tokens
/// start with `vv_`, so one can never be mistaken for the other.
pub const TOKEN_PREFIX: &str = "vvs_";
/// How long one webhook delivery may take.
pub const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(5);
const WEBHOOK_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Deliveries of one announcement in flight at once.
pub const SEND_CONCURRENCY: usize = 8;

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

// ---------------------------------------------------------------------------
// Link tokens and link base
// ---------------------------------------------------------------------------

/// The link token for `nonce`: an HMAC with the server's secret key, so the
/// database alone (which keeps only the nonce and the token's hash) is not
/// enough to confirm or unsubscribe anybody.
pub fn derive_token(key: &Key, nonce: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key.master()).expect("HMAC accepts any key length");
    mac.update(b"vv-subscriber-link\0");
    mac.update(nonce.as_bytes());
    format!("{TOKEN_PREFIX}{}", hex::encode(mac.finalize().into_bytes()))
}

/// A fresh link secret for a new subscriber.
pub fn new_secret(key: &Key) -> db::Secret {
    let mut raw = [0u8; 16];
    rand::fill(&mut raw);
    let nonce = hex::encode(raw);
    let token_hash = crate::auth::tokens::hash(&derive_token(key, &nonce));
    db::Secret { nonce, token_hash }
}

/// The link token of `sub`. A row from before tokens were derived (or from
/// before the key changed) gets a new secret, so its older links stop
/// working and the ones in this message work.
pub fn link_token(
    conn: &rusqlite::Connection,
    key: &Key,
    sub: &Subscriber,
) -> rusqlite::Result<String> {
    if !sub.nonce.is_empty() {
        let token = derive_token(key, &sub.nonce);
        if crate::auth::tokens::hash(&token) == sub.token_hash {
            return Ok(token);
        }
    }
    let secret = new_secret(key);
    db::set_secret(conn, sub.id, &secret)?;
    Ok(derive_token(key, &secret.nonce))
}

/// Cheap shape check before touching the database: a `vvs_` token, or an
/// older `vv_` one still in mails sent before tokens were derived.
pub fn is_well_formed(token: &str) -> bool {
    match token.strip_prefix(TOKEN_PREFIX) {
        Some(body) => body.len() == 64 && body.bytes().all(|b| b.is_ascii_hexdigit()),
        None => crate::auth::tokens::is_well_formed(token),
    }
}

/// Where links in messages point: the configured public URL, else the page's
/// custom domain over https. `None` means the page can't offer
/// subscriptions: there is no address that the server, rather than whoever
/// signs up, vouches for.
pub fn link_base(configured: Option<&str>, page: &StatusPage) -> Option<String> {
    configured
        .map(|b| b.trim().trim_end_matches('/'))
        .filter(|b| !b.is_empty())
        .map(str::to_string)
        .or_else(|| {
            page.custom_domain
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(|d| format!("https://{d}"))
        })
}

/// [`link_base`] for page `page` with the stored settings.
pub fn link_base_for(
    conn: &rusqlite::Connection,
    page: &StatusPage,
) -> rusqlite::Result<Option<String>> {
    let configured = crate::db::settings::load(conn)?.base_url;
    Ok(link_base(configured.as_deref(), page))
}

// ---------------------------------------------------------------------------
// Sending
// ---------------------------------------------------------------------------

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
    /// Confirmed subscribers with their link tokens.
    subscribers: Vec<(Subscriber, String)>,
    mail: Option<Value>,
    webhooks: bool,
    base: String,
}

/// Loads everything a send needs; `None` when the page doesn't (or no longer)
/// offer subscriptions. A page that became password protected keeps telling
/// the subscribers who confirmed while it was open: they opted in.
fn plan(conn: &rusqlite::Connection, key: &Key, page_id: i64) -> rusqlite::Result<Option<Plan>> {
    let Some(page) = db_pages::get(conn, page_id)? else {
        return Ok(None);
    };
    let settings = db::settings(conn, page_id)?;
    if !settings.enabled || !page.published {
        return Ok(None);
    }
    let Some(base) = link_base_for(conn, &page)? else {
        tracing::warn!(
            page = page.id,
            "status page subscribers not notified: set the public URL under Settings → General"
        );
        return Ok(None);
    };
    let mut subscribers = Vec::new();
    for sub in db::list_confirmed(conn, page_id)? {
        let token = link_token(conn, key, &sub)?;
        subscribers.push((sub, token));
    }
    Ok(Some(Plan {
        subscribers,
        mail: mail_config(conn, settings.channel_id)?,
        webhooks: settings.webhooks,
        base,
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

/// Runs `send` for every item, at most [`SEND_CONCURRENCY`] at a time, so
/// one slow mail server or webhook doesn't hold up everybody else.
async fn send_all<T, F, Fut>(items: Vec<T>, send: F)
where
    F: Fn(T) -> Fut,
    Fut: Future<Output = ()>,
{
    futures_util::stream::iter(items)
        .for_each_concurrent(SEND_CONCURRENCY, send)
        .await;
}

async fn run(state: &AppState, a: Announcement) -> Result<(), String> {
    let page_id = a.page_id;
    let key = state.session_key.clone();
    let Some(mut plan) = state
        .with_conn(move |c| plan(c, &key, page_id))
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(());
    };
    let subscribers = std::mem::take(&mut plan.subscribers);
    let plan = &plan;
    let a = &a;
    send_all(subscribers, |(sub, token)| async move {
        let lang = Lang::from_code(&sub.lang).unwrap_or_else(crate::i18n::site_default);
        let sent = crate::i18n::scope(lang, deliver(plan, &sub, &token, a)).await;
        if let Err(e) = &sent {
            tracing::warn!(page = plan.page.id, subscriber = sub.id, kind = %sub.kind, error = %e, "subscriber delivery failed");
        }
        let (id, ok) = (sub.id, sent.is_ok());
        let _ = state
            .with_conn(move |c| db::record_delivery(c, id, ok))
            .await;
    })
    .await;
    Ok(())
}

async fn deliver(
    plan: &Plan,
    sub: &Subscriber,
    token: &str,
    a: &Announcement,
) -> Result<(), String> {
    let links = Links::new(&plan.base, &plan.page, token);
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

/// Loads the page's plan and subscriber `id` with its link token.
async fn plan_for_one(
    state: &AppState,
    page_id: i64,
    id: i64,
) -> Result<Option<(Plan, Subscriber, String)>, String> {
    let key = state.session_key.clone();
    state
        .with_conn(move |c| {
            let Some(mut plan) = plan(c, &key, page_id)? else {
                return Ok(None);
            };
            plan.subscribers.clear();
            let Some(sub) = db::get(c, id)?.filter(|s| s.page_id == page_id) else {
                return Ok(None);
            };
            let token = link_token(c, &key, &sub)?;
            Ok(Some((plan, sub, token)))
        })
        .await
        .map_err(|e| e.to_string())
}

/// Mails the confirmation link to email sign-up `id`. (Webhooks get no
/// confirmation: an editor approves them, see [`send_welcome`].)
pub async fn send_confirmation(state: &AppState, page_id: i64, id: i64) -> Result<(), String> {
    let Some((plan, sub, token)) = plan_for_one(state, page_id, id).await? else {
        return Ok(());
    };
    if sub.kind != "email" || sub.is_confirmed() {
        return Ok(());
    }
    let links = Links::new(&plan.base, &plan.page, &token);
    let lang = Lang::from_code(&sub.lang).unwrap_or_else(crate::i18n::site_default);
    crate::i18n::scope(lang, async {
        let config = plan.mail.as_ref().ok_or("no email channel")?;
        let message = confirmation_mail(config, &plan.page, &sub.target, &links)?;
        crate::notification::email::deliver(config, message).await
    })
    .await
}

/// Tells an approved webhook that it is subscribed, with its unsubscribe link.
pub async fn send_welcome(state: &AppState, page_id: i64, id: i64) -> Result<(), String> {
    let Some((plan, sub, token)) = plan_for_one(state, page_id, id).await? else {
        return Ok(());
    };
    if sub.kind != "webhook" || !plan.webhooks {
        return Ok(());
    }
    let links = Links::new(&plan.base, &plan.page, &token);
    let body = json!({
        "type": "subscribed",
        "page": {"title": plan.page.title, "url": links.page},
        "unsubscribe_url": links.unsubscribe,
    });
    send_webhook(&sub.target, &body).await
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
        // Mail clients show an "unsubscribe" button for these headers. A GET
        // of the link opens a confirmation page, so a scanner prefetching it
        // unsubscribes nobody; the client's one-click POST (RFC 8058)
        // unsubscribes right away.
        b = b
            .raw_header(header::HeaderValue::new(
                header::HeaderName::new_from_ascii_str("List-Unsubscribe"),
                format!("<{url}>"),
            ))
            .raw_header(header::HeaderValue::new(
                header::HeaderName::new_from_ascii_str("List-Unsubscribe-Post"),
                "List-Unsubscribe=One-Click".to_string(),
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

fn is_public_v4(v4: Ipv4Addr) -> bool {
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
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // IETF protocol assignments
        || (o[0] == 192 && o[1] == 88 && o[2] == 99) // 6to4 relay anycast
        || (o[0] == 198 && (18..20).contains(&o[1])) // benchmarking
        || o[0] >= 240)
}

/// The IPv4 address a translated or tunnelled IPv6 address leads to.
fn embedded_v4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let seg = v6.segments();
    let o = v6.octets();
    if let Some(v4) = v6.to_ipv4_mapped() {
        return Some(v4);
    }
    // NAT64, well-known prefix 64:ff9b::/96.
    if seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return Some(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    // 6to4, 2002::/16: the IPv4 address follows the prefix.
    if seg[0] == 0x2002 {
        return Some(Ipv4Addr::new(o[2], o[3], o[4], o[5]));
    }
    None
}

/// Whether `ip` is a globally routable unicast address.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = embedded_v4(v6) {
                return is_public_v4(v4);
            }
            let seg = v6.segments();
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || seg[..6] == [0; 6] // IPv4-compatible ::a.b.c.d (deprecated)
                || (seg[0] == 0x64 && seg[1] == 0xff9b) // local-use NAT64 64:ff9b:1::/48
                || (seg[0] == 0x2001 && seg[1] == 0) // Teredo 2001::/32
                || (seg[0] & 0xfe00) == 0xfc00 // unique local
                || (seg[0] & 0xffc0) == 0xfe80 // link local
                || (seg[0] & 0xffc0) == 0xfec0 // site local (deprecated)
                || (seg[0] == 0x2001 && seg[1] == 0x0db8)) // documentation
        }
    }
}

/// Ports a webhook may use: the web's own, or any unprivileged one (never
/// SSH, SMTP, databases and the like on low ports).
fn allowed_port(port: u16) -> bool {
    matches!(port, 80 | 443) || port > 1024
}

/// Parses a subscriber-supplied webhook URL (syntax only: http(s), a host,
/// no credentials, port 80, 443 or above 1024).
pub fn parse_webhook_url(raw: &str) -> Option<url::Url> {
    let url = url::Url::parse(raw.trim()).ok()?;
    let ok = matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default().is_some_and(allowed_port)
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
    post_pinned(url, &addrs, body, WEBHOOK_TIMEOUT).await
}

/// POSTs `body`, connecting only to `addrs` (what was just checked): never
/// through a proxy from the environment, and following no redirects, either
/// of which could lead somewhere internal.
#[doc(hidden)]
pub async fn post_pinned(
    url: url::Url,
    addrs: &[SocketAddr],
    body: &Value,
    timeout: Duration,
) -> Result<(), String> {
    let mut client = crate::tls_roots::http_client()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(WEBHOOK_CONNECT_TIMEOUT.min(timeout))
        .timeout(timeout)
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

/// One key's counter.
#[derive(Debug, Clone, Copy)]
struct Window {
    start: Instant,
    /// The window this key was counted with (keys differ: per address, per
    /// target, per page).
    length: Duration,
    count: u32,
}

/// Fixed-window counters with a bounded number of keys.
#[derive(Debug)]
struct Throttle {
    map: HashMap<String, Window>,
    capacity: usize,
}

impl Throttle {
    fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::new(),
            capacity,
        }
    }

    fn hit(&mut self, key: &str, limit: u32, window: Duration, now: Instant) -> bool {
        if !self.map.contains_key(key) && self.map.len() >= self.capacity {
            // Forget finished windows, each by its own length; if that's not
            // enough, the oldest eighth — never everybody at once, which
            // would hand an attacker fresh budgets for every key.
            self.map
                .retain(|_, w| now.duration_since(w.start) < w.length);
            if self.map.len() >= self.capacity {
                let mut starts: Vec<Instant> = self.map.values().map(|w| w.start).collect();
                let cut = (self.capacity / 8).max(1).min(starts.len() - 1);
                let (_, cutoff, _) = starts.select_nth_unstable(cut);
                let cutoff = *cutoff;
                self.map.retain(|_, w| w.start > cutoff);
            }
        }
        let entry = self.map.entry(key.to_string()).or_insert(Window {
            start: now,
            length: window,
            count: 0,
        });
        if now.duration_since(entry.start) >= entry.length {
            *entry = Window {
                start: now,
                length: window,
                count: 0,
            };
        }
        entry.count = entry.count.saturating_add(1);
        entry.count <= limit
    }
}

static ATTEMPTS: LazyLock<Mutex<Throttle>> = LazyLock::new(|| Mutex::new(Throttle::new(20_000)));

/// Counts one attempt for `key`; `false` once `limit` attempts happened
/// within `window`.
pub fn allow(key: &str, limit: u32, window: Duration) -> bool {
    // A panic elsewhere while holding the lock leaves valid counters behind:
    // keep counting with them instead of letting every attempt through.
    ATTEMPTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .hit(key, limit, window, Instant::now())
}

/// The part of a client address that one party controls: the IPv4 address,
/// or the /64 an IPv6 host normally gets (it can pick any address inside).
pub fn client_key(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        },
    }
}

#[cfg(test)]
mod tests;
