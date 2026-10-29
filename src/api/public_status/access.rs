//! Password-protected status pages.
//!
//! A protected page shows an unlock form instead of its content. The right
//! password sets a signed, HttpOnly, `SameSite=Lax` cookie scoped to that page
//! (`/status/{slug}`, or `/` on its custom domain) for 30 days. The cookie
//! carries the page's `password_version`, so changing or removing the
//! password revokes every cookie handed out before. Logged-in users bypass
//! the lock (they can see every monitor in the app anyway).

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, SameSite, SignedCookieJar};
use serde::Deserialize;

use super::PublicPage;
use crate::api::auth::{PeerAddr, secure_cookies};
use crate::api::{ApiError, ApiResult, AppState, HtmlTemplate};
use crate::auth::rate_limit::{Policy, RateLimiter};
use crate::auth::session;
use crate::db::status_pages::{self as db_pages, StatusPage};
use crate::i18n::Tr as _;

/// How long an unlock lasts.
pub const UNLOCK_TTL_SECS: i64 = 30 * 24 * 3600;

/// Per page and client IP: 5 free guesses, then 2 s, 4 s, 8 s … up to 15 minutes.
static UNLOCK_LIMITER: LazyLock<RateLimiter> = LazyLock::new(|| {
    RateLimiter::new(Policy {
        free_failures: 5,
        base_delay: Duration::from_secs(2),
        max_delay: Duration::from_secs(15 * 60),
        forget_after: Duration::from_secs(60 * 60),
    })
});

pub fn cookie_name(page_id: i64) -> String {
    format!("vv_sp_{page_id}")
}

/// `"{page_id}.{password_version}.{issued_unix}"`, signed by the jar.
fn cookie_value(page: &StatusPage, now: i64) -> String {
    format!("{}.{}.{}", page.id, page.password_version, now)
}

/// Whether `value` (already signature-checked) unlocks `page` at `now`.
pub fn value_unlocks(value: &str, page: &StatusPage, now: i64) -> bool {
    let mut parts = value.split('.');
    let (Some(id), Some(version), Some(issued), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let (Ok(id), Ok(version), Ok(issued)) = (
        id.parse::<i64>(),
        version.parse::<i64>(),
        issued.parse::<i64>(),
    ) else {
        return false;
    };
    id == page.id
        && version == page.password_version
        && issued <= now + 60
        && now - issued < UNLOCK_TTL_SECS
}

/// Whether the visitor holds a valid unlock cookie for `page`.
pub fn is_unlocked(jar: &SignedCookieJar, page: &StatusPage) -> bool {
    jar.get(&cookie_name(page.id))
        .is_some_and(|c| value_unlocks(c.value(), page, session::now_unix()))
}

/// What a visitor may see of a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Access {
    pub logged_in: bool,
    pub unlocked: bool,
}

impl Access {
    /// Whether the content of `page` may be shown.
    pub fn allows(self, page: &StatusPage) -> bool {
        !page.is_protected() || self.logged_in || self.unlocked
    }
}

pub fn access_for(jar: &SignedCookieJar, page: &StatusPage, logged_in: bool) -> Access {
    Access {
        logged_in,
        unlocked: page.is_protected() && is_unlocked(jar, page),
    }
}

/// The request's host name (lowercase, no port).
fn host_of(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::HOST)?.to_str().ok()?;
    let host = if raw.starts_with('[') {
        raw
    } else {
        raw.rsplit_once(':').map_or(raw, |(h, _)| h)
    };
    Some(host.trim_end_matches('.').to_ascii_lowercase())
}

/// Whether the request reached `page` through its custom domain.
pub fn on_custom_domain(headers: &HeaderMap, page: &StatusPage) -> bool {
    page.custom_domain
        .as_deref()
        .is_some_and(|d| host_of(headers).as_deref() == Some(d))
}

/// Where the page lives for this request (and the unlock cookie's path).
fn page_path(headers: &HeaderMap, page: &StatusPage) -> String {
    if on_custom_domain(headers, page) {
        "/".into()
    } else {
        format!("/status/{}", page.slug)
    }
}

pub fn unlock_cookie(page: &StatusPage, path: String, secure: bool, now: i64) -> Cookie<'static> {
    let mut c = Cookie::new(cookie_name(page.id), cookie_value(page, now));
    c.set_path(path);
    c.set_http_only(true);
    c.set_same_site(SameSite::Lax);
    c.set_secure(secure);
    c.set_max_age(time::Duration::seconds(UNLOCK_TTL_SECS));
    c
}

#[derive(Template)]
#[template(path = "public_status_unlock.html")]
struct UnlockTemplate {
    page_title: String,
    theme: String,
    custom_css: Option<String>,
    page: StatusPage,
    error: Option<String>,
}

/// The unlock form for a locked page (`status` 401, or 429 while throttled).
pub fn locked_response(page: &StatusPage, error: Option<String>, status: StatusCode) -> Response {
    let tpl = UnlockTemplate {
        page_title: page.title.clone(),
        theme: page.theme.clone(),
        custom_css: page.custom_css.clone().filter(|c| !c.trim().is_empty()),
        page: page.clone(),
        error,
    };
    let mut resp = (status, HtmlTemplate(tpl)).into_response();
    resp.extensions_mut().insert(PublicPage);
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert("x-robots-tag", HeaderValue::from_static("noindex"));
    resp
}

fn wait_message(wait: Duration) -> String {
    let secs = wait.as_secs().max(1);
    if secs < 90 {
        crate::i18n::tf("sec.too_many_secs", &[("n", &secs.to_string())])
    } else {
        crate::i18n::tf(
            "sec.too_many_mins",
            &[("n", &secs.div_ceil(60).to_string())],
        )
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct UnlockForm {
    #[serde(default)]
    password: String,
}

/// `POST /status/{slug}/unlock`. Same-origin like every form post (the CSRF
/// guard checks `Origin`), rate-limited per page and client IP.
pub async fn unlock(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    PeerAddr(peer): PeerAddr,
    Path(slug): Path<String>,
    Form(form): Form<UnlockForm>,
) -> ApiResult<Response> {
    let page = state
        .with_conn(move |c| db_pages::get_by_slug(c, &slug))
        .await?
        .filter(|p| p.published)
        .ok_or(ApiError::NotFound)?;
    let target = page_path(&headers, &page);
    let Some(hash) = page.password_hash.clone() else {
        return Ok(Redirect::to(&target).into_response());
    };
    let ip = session::client_ip(&headers, peer, session::trust_proxy())
        .map_or_else(|| "unknown".to_string(), |ip| ip.to_string());
    let key = format!("sp:{}:{ip}", page.id);
    if let Err(wait) = UNLOCK_LIMITER.reserve(&key, Instant::now()) {
        tracing::warn!(page = page.id, "status page unlock throttled");
        return Ok(locked_response(
            &page,
            Some(wait_message(wait)),
            StatusCode::TOO_MANY_REQUESTS,
        ));
    }
    let ok = form.password.len() <= crate::auth::MAX_PASSWORD_LEN
        && crate::auth::verify_password_async(hash, form.password).await?;
    if !ok {
        return Ok(locked_response(
            &page,
            Some(crate::i18n::t("spx.wrong_password").into()),
            StatusCode::UNAUTHORIZED,
        ));
    }
    UNLOCK_LIMITER.clear(&key);
    let cookie = unlock_cookie(
        &page,
        target.clone(),
        secure_cookies(&headers),
        session::now_unix(),
    );
    Ok((jar.add(cookie), Redirect::to(&target)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(version: i64) -> StatusPage {
        StatusPage {
            id: 7,
            slug: "acme".into(),
            title: "Acme".into(),
            description: String::new(),
            custom_domain: Some("status.acme.test".into()),
            custom_domain_only: false,
            published: true,
            theme: "auto".into(),
            show_tags: false,
            show_uptime: true,
            show_response: true,
            show_powered_by: true,
            footer_text: None,
            custom_css: None,
            refresh_seconds: 60,
            created_at: String::new(),
            updated_at: String::new(),
            password_hash: Some("x".into()),
            password_version: version,
            analytics: Default::default(),
            logo_sha: None,
            favicon_sha: None,
        }
    }

    #[test]
    fn cookie_values_are_bound_to_page_version_and_age() {
        let now = 1_800_000_000;
        let p = page(3);
        assert!(value_unlocks(&cookie_value(&p, now), &p, now));
        // Another page, a newer password, expired or from the future: no.
        assert!(!value_unlocks(
            &cookie_value(&p, now),
            &StatusPage { id: 8, ..page(3) },
            now
        ));
        assert!(!value_unlocks(&cookie_value(&p, now), &page(4), now));
        assert!(!value_unlocks(
            &cookie_value(&p, now - UNLOCK_TTL_SECS),
            &p,
            now
        ));
        assert!(!value_unlocks(&cookie_value(&p, now + 3600), &p, now));
        for junk in ["", "7.3", "7.3.x", "7.3.1.1", "a.b.c"] {
            assert!(!value_unlocks(junk, &p, now), "{junk}");
        }
    }

    #[test]
    fn cookie_is_scoped_http_only_and_lax() {
        let c = unlock_cookie(&page(1), "/status/acme".into(), true, 1);
        assert_eq!(c.path(), Some("/status/acme"));
        assert_eq!(c.http_only(), Some(true));
        assert_eq!(c.secure(), Some(true));
        assert_eq!(c.same_site(), Some(SameSite::Lax));
        assert_eq!(c.max_age(), Some(time::Duration::seconds(UNLOCK_TTL_SECS)));
    }

    #[test]
    fn custom_domain_requests_get_the_root_path() {
        let mut h = HeaderMap::new();
        h.insert(
            header::HOST,
            HeaderValue::from_static("Status.Acme.Test:443"),
        );
        assert_eq!(page_path(&h, &page(1)), "/");
        h.insert(header::HOST, HeaderValue::from_static("vole.example.com"));
        assert_eq!(page_path(&h, &page(1)), "/status/acme");
    }
}
