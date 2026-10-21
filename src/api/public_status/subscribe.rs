//! Public subscription pages: `/status/{slug}/subscribe[/confirm|unsubscribe/{token}]`.
//!
//! Signing up always answers the same ("check your inbox") whether or not
//! the address was already known, so the form can't be used to find out who
//! subscribes. Confirming and unsubscribing are a page with a button (a POST),
//! so a mail scanner that opens the links does neither. Webhook sign-ups get
//! no confirmation at all: they wait for an editor's approval.
//!
//! A page only offers subscriptions when links can point at an address the
//! server vouches for: the configured public URL or the page's custom domain
//! ([`send::link_base`]).

use crate::i18n::Tr as _;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;
use std::str::FromStr;
use std::time::Duration;

use super::{PublicPage, access};
use crate::api::auth::PeerAddr;
use crate::api::{ApiError, ApiResult, AppState, HtmlTemplate};
use crate::auth::session;
use crate::db::status_pages::StatusPage;
use crate::db::subscribers::{self as db, Added, Settings};
use crate::subscribers as send;

#[derive(askama::Template)]
#[template(path = "public_status_subscribe.html")]
struct SubscribeTemplate {
    page_title: String,
    theme: String,
    custom_css: Option<String>,
    page: StatusPage,
    /// "form" | "sent" | "review" | "confirm" | "confirmed" | "unsubscribe" | "unsubscribed" | "invalid"
    mode: &'static str,
    error: Option<String>,
    email: bool,
    webhook: bool,
    /// Masked target, on the confirm/unsubscribe pages.
    target: String,
    token: String,
}

impl SubscribeTemplate {
    fn new(page: StatusPage, mode: &'static str) -> Self {
        Self {
            page_title: page.title.clone(),
            theme: page.theme.clone(),
            custom_css: page.custom_css.clone().filter(|c| !c.trim().is_empty()),
            page,
            mode,
            error: None,
            email: false,
            webhook: false,
            target: String::new(),
            token: String::new(),
        }
    }

    fn respond(self, status: StatusCode) -> Response {
        let mut resp = (status, HtmlTemplate(self)).into_response();
        resp.extensions_mut().insert(PublicPage);
        let h = resp.headers_mut();
        h.insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
        h.insert("x-robots-tag", header::HeaderValue::from_static("noindex"));
        resp
    }
}

/// Published page whose visitor may see it and that offers subscriptions,
/// with its subscription settings.
async fn open_page(
    state: &AppState,
    jar: &SignedCookieJar,
    slug: String,
) -> ApiResult<Result<(StatusPage, Settings), Response>> {
    let (view, access) = super::resolve(state, slug, jar).await?;
    let page = view.page;
    if !access.allows(&page) {
        return Ok(Err(access::locked_response(
            &page,
            None,
            StatusCode::UNAUTHORIZED,
        )));
    }
    let p = page.clone();
    let (settings, base) = state
        .with_conn(move |c| Ok((db::settings(c, p.id)?, send::link_base_for(c, &p)?)))
        .await?;
    if !settings.enabled || base.is_none() {
        return Err(ApiError::NotFound);
    }
    Ok(Ok((page, settings)))
}

fn offers(settings: &Settings, mail_ok: bool) -> (bool, bool) {
    (settings.channel_id.is_some() && mail_ok, settings.webhooks)
}

/// Whether the page's mail channel is a usable email channel.
async fn mail_ok(state: &AppState, settings: &Settings) -> bool {
    let Some(id) = settings.channel_id else {
        return false;
    };
    state
        .with_conn(move |c| crate::db::notifications::get_channel(c, id))
        .await
        .ok()
        .flatten()
        .is_some_and(|c| c.active && c.kind == "email")
}

async fn form_page(state: &AppState, page: StatusPage, settings: &Settings) -> SubscribeTemplate {
    let (email, webhook) = offers(settings, mail_ok(state, settings).await);
    let mut tpl = SubscribeTemplate::new(page, "form");
    tpl.email = email;
    tpl.webhook = webhook;
    tpl
}

pub async fn form(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Path(slug): Path<String>,
) -> ApiResult<Response> {
    let (page, settings) = match open_page(&state, &jar, slug).await? {
        Ok(v) => v,
        Err(resp) => return Ok(resp),
    };
    Ok(form_page(&state, page, &settings)
        .await
        .respond(StatusCode::OK))
}

#[derive(Debug, Deserialize)]
pub struct SignUp {
    #[serde(default)]
    email: String,
    #[serde(default)]
    webhook: String,
    /// Honeypot: hidden from people, filled in by bots.
    #[serde(default)]
    website: String,
}

fn valid_email(raw: &str) -> bool {
    raw.len() <= 254 && !raw.contains(char::is_whitespace) && lettre::Address::from_str(raw).is_ok()
}

/// New sign-ups one page takes per hour, whatever addresses they come from.
pub const PAGE_SIGNUPS_PER_HOUR: u32 = 100;

pub async fn sign_up(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    PeerAddr(peer): PeerAddr,
    Path(slug): Path<String>,
    Form(form): Form<SignUp>,
) -> ApiResult<Response> {
    let (page, settings) = match open_page(&state, &jar, slug).await? {
        Ok(v) => v,
        Err(resp) => return Ok(resp),
    };
    let (email_on, webhook_on) = offers(&settings, mail_ok(&state, &settings).await);
    let mut again = SubscribeTemplate::new(page.clone(), "form");
    again.email = email_on;
    again.webhook = webhook_on;

    let ip = session::client_ip(&headers, peer, session::trust_proxy())
        .map_or_else(|| "unknown".to_string(), send::client_key);
    // The data directory tells app instances apart (tests run several).
    let scope = state.config.data_dir.display();
    if !send::allow(
        &format!("sub-ip:{scope}:{ip}"),
        20,
        Duration::from_secs(600),
    ) {
        again.error = Some(crate::i18n::t("sub.err_rate").into());
        return Ok(again.respond(StatusCode::TOO_MANY_REQUESTS));
    }
    if !form.website.is_empty() {
        // A bot: answer like a success, do nothing.
        return Ok(SubscribeTemplate::new(page, "sent").respond(StatusCode::OK));
    }

    let email = form.email.trim().to_string();
    let hook = form.webhook.trim().to_string();
    let (kind, target) = if !email.is_empty() && email_on {
        if !valid_email(&email) {
            again.error = Some(crate::i18n::t("sub.err_email").into());
            return Ok(again.respond(StatusCode::UNPROCESSABLE_ENTITY));
        }
        ("email", email.to_lowercase())
    } else if !hook.is_empty() && webhook_on {
        let ok = match send::parse_webhook_url(&hook) {
            Some(url) => send::resolve_public(&url).await.is_ok(),
            None => false,
        };
        if !ok {
            again.error = Some(crate::i18n::t("sub.err_webhook").into());
            return Ok(again.respond(StatusCode::UNPROCESSABLE_ENTITY));
        }
        ("webhook", hook)
    } else {
        again.error = Some(
            crate::i18n::t(if email_on {
                "sub.err_email"
            } else {
                "sub.err_webhook"
            })
            .into(),
        );
        return Ok(again.respond(StatusCode::UNPROCESSABLE_ENTITY));
    };

    // However many addresses they come from, sign-ups can't flood a page.
    if !send::allow(
        &format!("sub-page:{scope}:{}", page.id),
        PAGE_SIGNUPS_PER_HOUR,
        Duration::from_secs(3600),
    ) {
        again.error = Some(crate::i18n::t("sub.err_rate").into());
        return Ok(again.respond(StatusCode::TOO_MANY_REQUESTS));
    }
    let done = if kind == "email" { "sent" } else { "review" };
    // One address can't be used to spam someone with confirmation mails.
    if !send::allow(
        &format!("sub-target:{scope}:{}:{kind}:{target}", page.id),
        3,
        Duration::from_secs(3600),
    ) {
        return Ok(SubscribeTemplate::new(page, done).respond(StatusCode::OK));
    }

    let (page_id, lang, secret) = (
        page.id,
        crate::i18n::current().code(),
        send::new_secret(&state.session_key),
    );
    let added = state
        .with_conn(move |c| db::add(c, page_id, kind, &target, lang, &secret))
        .await?;
    if let (Added::Pending(id), "email") = (added, kind) {
        // In the background: a slow mail server must not make the answer
        // slower (or tell a new address from a known one by timing).
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = send::send_confirmation(&state, page_id, id).await {
                tracing::warn!(page = page_id, error = %e, "subscription confirmation failed");
            }
        });
    }
    Ok(SubscribeTemplate::new(page, done).respond(StatusCode::OK))
}

/// Looks the token up and the page it belongs to.
async fn by_token(
    state: &AppState,
    slug: String,
    token: String,
) -> ApiResult<Option<(StatusPage, db::Subscriber)>> {
    if !send::is_well_formed(&token) {
        return Ok(None);
    }
    state
        .with_conn(move |c| {
            let Some(sub) = db::by_token(c, &token)? else {
                return Ok(None);
            };
            let Some(page) = crate::db::status_pages::get(c, sub.page_id)? else {
                return Ok(None);
            };
            Ok((page.slug == slug && page.published).then_some((page, sub)))
        })
        .await
}

fn invalid(state_page: StatusPage) -> Response {
    let mut tpl = SubscribeTemplate::new(state_page, "invalid");
    tpl.error = Some(crate::i18n::t("sub.invalid_link").into());
    tpl.respond(StatusCode::NOT_FOUND)
}

async fn page_for_error(state: &AppState, slug: String) -> ApiResult<Response> {
    let page = state
        .with_conn(move |c| crate::db::status_pages::get_by_slug(c, &slug))
        .await?
        .filter(|p| p.published)
        .ok_or(ApiError::NotFound)?;
    Ok(invalid(page))
}

async fn token_page(
    state: &AppState,
    slug: String,
    token: String,
    mode: &'static str,
) -> ApiResult<Response> {
    let Some((page, sub)) = by_token(state, slug.clone(), token.clone()).await? else {
        return page_for_error(state, slug).await;
    };
    // Only an email address is confirmed by its link; webhooks wait for an editor.
    if mode == "confirm" && sub.kind != "email" {
        return Ok(invalid(page));
    }
    let mut tpl = SubscribeTemplate::new(page, mode);
    tpl.target = sub.masked_target();
    tpl.token = token;
    Ok(tpl.respond(StatusCode::OK))
}

pub async fn confirm_page(
    State(state): State<AppState>,
    Path((slug, token)): Path<(String, String)>,
) -> ApiResult<Response> {
    token_page(&state, slug, token, "confirm").await
}

pub async fn confirm(
    State(state): State<AppState>,
    Path((slug, token)): Path<(String, String)>,
) -> ApiResult<Response> {
    let Some((page, _)) = by_token(&state, slug.clone(), token.clone()).await? else {
        return page_for_error(&state, slug).await;
    };
    let confirmed = state.with_conn(move |c| db::confirm(c, &token)).await?;
    Ok(match confirmed {
        Some(_) => SubscribeTemplate::new(page, "confirmed").respond(StatusCode::OK),
        None => invalid(page),
    })
}

pub async fn unsubscribe_page(
    State(state): State<AppState>,
    Path((slug, token)): Path<(String, String)>,
) -> ApiResult<Response> {
    token_page(&state, slug, token, "unsubscribe").await
}

pub async fn unsubscribe(
    State(state): State<AppState>,
    Path((slug, token)): Path<(String, String)>,
) -> ApiResult<Response> {
    let Some((page, _)) = by_token(&state, slug.clone(), token.clone()).await? else {
        // Already gone: say so as if it worked, the goal is reached.
        let page = state
            .with_conn(move |c| crate::db::status_pages::get_by_slug(c, &slug))
            .await?
            .filter(|p| p.published)
            .ok_or(ApiError::NotFound)?;
        return Ok(SubscribeTemplate::new(page, "unsubscribed").respond(StatusCode::OK));
    };
    state.with_conn(move |c| db::unsubscribe(c, &token)).await?;
    Ok(SubscribeTemplate::new(page, "unsubscribed").respond(StatusCode::OK))
}
