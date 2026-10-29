//! The request-level half of authentication: the middleware that gates
//! protected routes and guards against cross-site requests, and the
//! helpers that turn a cookie or an API token into a [`CurrentUser`].
//! These need the database, so they take [`Core`].

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::SignedCookieJar;

use super::session::{self, SessionClaims};
use super::{AuthMethod, CurrentUser, Scopes, csrf, tokens};
use crate::db::api_tokens;
use crate::db::users::{self, User};
use crate::http::{ApiError, ApiResult};
use crate::state::Core;

fn session_cookie() -> &'static str {
    crate::config::get().session_cookie
}

fn pending_2fa_cookie() -> &'static str {
    crate::config::get().pending_2fa_cookie
}

/// TCP peer address when the server was started with connect info.
pub struct PeerAddr(pub Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for PeerAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(PeerAddr(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ci| ci.0),
        ))
    }
}

/// Should cookies set in response to this request carry `Secure`? Yes when
/// the request came in over HTTPS (behind a trusted proxy) or when the
/// configured public URL is https, however the request got here.
pub fn secure_cookies(headers: &HeaderMap) -> bool {
    session::is_https(headers, session::trust_proxy()) || session::base_url_is_https()
}

pub fn current_user_from(user: &User) -> CurrentUser {
    CurrentUser {
        id: user.id,
        username: user.username.clone(),
        display_name: user.name().to_string(),
        role: user.role,
        account_role: user.role,
        scopes: Scopes::All,
    }
}

pub async fn has_users(core: &Core) -> ApiResult<bool> {
    Ok(core.with_conn(|c| users::count_users(c)).await? > 0)
}

/// Adds a fresh session cookie for `user` (and drops any pending-2FA cookie).
/// A product with `CredentialMode::External` also drops an earlier
/// [re-auth grant](super::reauth), so a new session starts unconfirmed; grant
/// again after this call if the sign-in itself confirmed the user.
pub fn login_jar(
    jar: SignedCookieJar,
    user_id: i64,
    session_version: i64,
    secure: bool,
) -> SignedCookieJar {
    let claims = SessionClaims::new(user_id, session_version, session::now_unix());
    let jar = jar
        .add(session::session_cookie(&claims, secure))
        .remove(session::removal_cookie(pending_2fa_cookie()));
    match crate::config::get().features.credentials {
        crate::config::CredentialMode::Plain => jar,
        crate::config::CredentialMode::External(_) => super::reauth::clear(jar),
    }
}

/// Resolves the session cookie: claims must be well-formed, not idle-expired,
/// and match the user's current `session_version`; the user must be active.
pub async fn resolve_session(
    core: &Core,
    jar: &SignedCookieJar,
) -> ApiResult<Option<(CurrentUser, SessionClaims)>> {
    Ok(resolve_session_lang(core, jar)
        .await?
        .map(|(user, claims, _)| (user, claims)))
}

/// Like [`resolve_session`], plus the user's own interface language.
pub async fn resolve_session_lang(
    core: &Core,
    jar: &SignedCookieJar,
) -> ApiResult<Option<(CurrentUser, SessionClaims, Option<crate::i18n::Lang>)>> {
    let now = session::now_unix();
    let Some(claims) = jar
        .get(session_cookie())
        .and_then(|c| SessionClaims::decode(c.value()))
        .filter(|c| c.is_live(now))
    else {
        return Ok(None);
    };
    let id = claims.user_id;
    let user = core.with_conn(move |c| users::get_user(c, id)).await?;
    Ok(user
        .filter(|u| u.active && u.session_version == claims.version)
        .map(|u| {
            let lang = u.language.as_deref().and_then(crate::i18n::Lang::from_code);
            (current_user_from(&u), claims, lang)
        }))
}

/// Resolves the session cookie to an active user, if any. Protected routes get
/// this via the `CurrentUser` extractor; public pages (e.g. a status page that
/// shows an "Edit" link to logged-in editors) can call it directly.
pub async fn session_user(core: &Core, jar: &SignedCookieJar) -> ApiResult<Option<CurrentUser>> {
    Ok(resolve_session(core, jar).await?.map(|(user, _)| user))
}

/// Resolves an API token to the user it acts as and how (see
/// [`authenticate_token`]). It also returns tokens that can't read (a
/// product scope without
/// [`implies_read`](crate::config::ExtraScope::implies_read)); callers that
/// serve reads check [`CurrentUser::can_read`], as `require_auth` does.
///
/// A product without [`KitFeatures::api_tokens`](crate::config::KitFeatures::api_tokens)
/// has no tokens: every one resolves to nobody.
pub async fn authenticate_token_detailed(
    core: &Core,
    token: &str,
) -> ApiResult<Option<(CurrentUser, AuthMethod)>> {
    if !core.cfg.features.api_tokens {
        return Ok(None);
    }
    if !tokens::is_well_formed(token) {
        return Ok(None);
    }
    let token_hash = tokens::hash(token);
    let declared = core.cfg.token_scopes;
    let Some((owner, stored)) = core
        .with_conn(move |c| {
            let Some(owner) = api_tokens::find_valid_by_hash(c, &token_hash)? else {
                return Ok(None);
            };
            // Only products with scopes have the table.
            let stored = if declared.is_empty() {
                Vec::new()
            } else {
                api_tokens::scopes_of(c, owner.token_id)?
            };
            Ok(Some((owner, stored)))
        })
        .await?
    else {
        return Ok(None);
    };
    let Some(scopes) = super::token_scopes(owner.scope, &stored, declared) else {
        return Ok(None);
    };
    if owner.needs_touch {
        let id = owner.token_id;
        if let Err(err) = core.with_conn(move |c| api_tokens::touch(c, id)).await {
            tracing::warn!(error = %err, "failed to record API token use");
        }
    }
    let display_name = if owner.display_name.trim().is_empty() {
        owner.username.clone()
    } else {
        owner.display_name
    };
    let user = CurrentUser {
        id: owner.user_id,
        username: owner.username,
        display_name,
        role: owner.scope.cap(owner.role),
        account_role: owner.role,
        scopes,
    };
    Ok(Some((
        user,
        AuthMethod::Token {
            token_id: owner.token_id,
            scope: owner.scope,
        },
    )))
}

/// Resolves a `vv_…` API token to the user it acts as, with the role capped by
/// the token's scope (read → viewer, write → the user's role). `None` for
/// unknown, malformed, expired or revoked tokens, disabled users and tokens
/// that can't read ([`CurrentUser::can_read`]). Records `last_used_at` (at
/// most once a minute).
pub async fn authenticate_token(core: &Core, token: &str) -> ApiResult<Option<CurrentUser>> {
    Ok(authenticate_token_detailed(core, token)
        .await?
        .map(|(user, _)| user)
        .filter(CurrentUser::can_read))
}

/// [`authenticate_token`] for `Authorization: Bearer <token>` or
/// `Authorization: Basic base64(any:<token>)` (Prometheus `basic_auth`).
/// `None` when the header is missing or the token is invalid.
pub async fn authenticate_headers(
    core: &Core,
    headers: &HeaderMap,
) -> ApiResult<Option<CurrentUser>> {
    match tokens::token_from_headers(headers) {
        Some(token) => authenticate_token(core, &token).await,
        None => Ok(None),
    }
}

fn is_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("hx-request")
}

/// Unauthenticated response: full-page GETs go to `/login?next=<here>`;
/// htmx/JSON callers get a 401 shaped by `http::error::negotiate`.
pub fn unauthorized(method: &Method, uri: &axum::http::Uri, headers: &HeaderMap) -> Response {
    let path = uri.path();
    if *method == Method::GET && !is_htmx(headers) && !path.starts_with("/api/") {
        let here = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
        let next = session::sanitize_next(Some(here));
        let target = if next == "/" {
            "/login".to_string()
        } else {
            format!(
                "/login?next={}",
                url::form_urlencoded::byte_serialize(next.as_bytes()).collect::<String>()
            )
        };
        return Redirect::to(&target).into_response();
    }
    ApiError::Unauthorized.into_response()
}

fn sets_session_cookie(response: &Response) -> bool {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.starts_with(&format!("{}=", session_cookie())))
}

/// Whether a cookie-authenticated request to a CSRF-exempt path may go
/// ahead: reads always, writes only with the proof of
/// [`session_write_allowed`](crate::http::rest::session_write_allowed).
async fn same_origin_write(core: &Core, method: &Method, headers: &HeaderMap) -> bool {
    use crate::http::rest::session_write_allowed;
    if session_write_allowed(method, headers, None) {
        return true;
    }
    // Second chance, as in `csrf_guard`: the configured public URL.
    let has_source = headers.contains_key(header::ORIGIN) || headers.contains_key(header::REFERER);
    has_source && session_write_allowed(method, headers, core.base_url().await.as_deref())
}

/// Gate for every protected route. Inserts [`CurrentUser`] and [`AuthMethod`]
/// into the request extensions.
///
/// - Under `/api/`, an `Authorization` header (Bearer or Basic) authenticates
///   with an API token, and only with it (no fallback to the cookie). A
///   token that can't read (its product scopes leave reading out) is
///   refused with 403 outside its scopes' paths.
/// - Otherwise the session cookie; its idle timer slides forward hourly.
///   On a CSRF-exempt path (`KitConfig::csrf_exempt_prefixes`, where tokens
///   are the norm) a cookie-authenticated write must also prove it comes
///   from our own pages, like `http::rest::session_write_guard`.
/// - A brand new install (no users) bootstraps to `/setup`.
pub async fn require_auth(
    State(core): State<Core>,
    jar: SignedCookieJar,
    mut request: Request,
    next: Next,
) -> Response {
    let is_api = request.uri().path().starts_with("/api/");

    if is_api && tokens::has_authorization(request.headers()) {
        let token = tokens::token_from_headers(request.headers());
        let resolved = match token {
            Some(token) => authenticate_token_detailed(&core, &token).await,
            None => Ok(None),
        };
        return match resolved {
            Ok(Some((user, _)))
                if !user.can_read() && !user.scope_allows_path(request.uri().path()) =>
            {
                ApiError::Forbidden.into_response()
            }
            Ok(Some((user, method))) => {
                request.extensions_mut().insert(user);
                request.extensions_mut().insert(method);
                next.run(request).await
            }
            Ok(None) => ApiError::Unauthorized.into_response(),
            Err(err) => err.into_response(),
        };
    }

    match resolve_session_lang(&core, &jar).await {
        Ok(Some((user, claims, lang))) => {
            // Token endpoints are exempt from the CSRF guard, but this
            // request carries the browser's cookie: it must prove it is ours.
            if csrf::is_exempt_path(request.uri().path())
                && !same_origin_write(&core, request.method(), request.headers()).await
            {
                tracing::warn!(
                    method = %request.method(),
                    path = %request.uri().path(),
                    "blocked cross-site write with a session cookie"
                );
                return if is_api {
                    crate::http::rest::session_write_refused().into_response()
                } else {
                    ApiError::Forbidden.into_response()
                };
            }
            request.extensions_mut().insert(user);
            request.extensions_mut().insert(AuthMethod::Session);
            let secure = secure_cookies(request.headers());
            // A user's own language wins over the site default.
            let lang = lang.unwrap_or_else(crate::i18n::current);
            let response = crate::i18n::scope(lang, next.run(request)).await;
            let now = session::now_unix();
            // Slide the idle timer, unless the handler just issued a new
            // session cookie itself (e.g. after a password change).
            if claims.needs_refresh(now) && !sets_session_cookie(&response) {
                let refreshed = claims.refreshed(now);
                let jar = SignedCookieJar::new(core.session_key.clone())
                    .add(session::session_cookie(&refreshed, secure));
                return (jar, response).into_response();
            }
            response
        }
        Ok(None) => {
            // A brand new install has nobody to log in as yet.
            match has_users(&core).await {
                Ok(true) => {}
                Ok(false) if !is_api => return Redirect::to("/setup").into_response(),
                Ok(false) => return ApiError::Unauthorized.into_response(),
                Err(err) => return err.into_response(),
            }
            let response = unauthorized(request.method(), request.uri(), request.headers());
            if jar.get(session_cookie()).is_some() {
                // Stale cookie (expired, signed out elsewhere, user disabled): drop it.
                return (
                    jar.remove(session::removal_cookie(session_cookie())),
                    response,
                )
                    .into_response();
            }
            response
        }
        Err(err) => err.into_response(),
    }
}

/// CSRF guard (runs on every route): state-changing requests must carry
/// `HX-Request: true` or a same-origin `Origin`/`Referer` — see
/// [`csrf`]. Token/public machine endpoints are exempt.
pub async fn csrf_guard(State(core): State<Core>, mut request: Request, next: Next) -> Response {
    // This layer wraps every route, so it is also where duplicate session cookies are
    // collapsed before any extractor reads them.
    dedupe_session_cookie(request.headers_mut(), &core.session_key);
    session::warn_if_proxied_without_trust(request.headers());
    // Every few seconds at most: whether the public URL is https decides
    // if cookies are `Secure` (read through `Core::base_url`).
    if session::base_url_stale() {
        core.base_url().await;
    }
    let trust = session::trust_proxy();
    let (method, path, headers) = (request.method(), request.uri().path(), request.headers());
    if csrf::is_allowed(method, path, headers, trust, None) {
        return next.run(request).await;
    }
    // Second chance: the configured public URL (a proxy that rewrites Host).
    let has_source = headers.contains_key(header::ORIGIN) || headers.contains_key(header::REFERER);
    if has_source {
        let base_url = core.base_url().await;
        if csrf::is_allowed(method, path, headers, trust, base_url.as_deref()) {
            return next.run(request).await;
        }
    }
    tracing::warn!(
        method = %method,
        path = %path,
        origin = ?headers.get(header::ORIGIN),
        referer = ?headers.get(header::REFERER),
        "blocked cross-site request"
    );
    ApiError::Forbidden.into_response()
}

/// Browsers can hold several `vv_session` cookies for one host at once (e.g. one left
/// over from an older install on another port, or set with a different Path/Domain).
/// The cookie jar keeps only the first one it sees, so a stale cookie would shadow every
/// fresh login. Keep the first value whose signature verifies and drop the others.
fn dedupe_session_cookie(headers: &mut HeaderMap, key: &axum_extra::extract::cookie::Key) {
    let prefix = format!("{}=", session_cookie());
    let pairs: Vec<String> = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if pairs.iter().filter(|p| p.starts_with(&prefix)).count() < 2 {
        return;
    }
    let valid = pairs
        .iter()
        .filter(|p| p.starts_with(&prefix))
        .find(|p| {
            let Ok(value) = axum::http::HeaderValue::from_str(p) else {
                return false;
            };
            let mut single = HeaderMap::new();
            single.insert(header::COOKIE, value);
            SignedCookieJar::from_headers(&single, key.clone())
                .get(session_cookie())
                .is_some()
        })
        .cloned();
    let merged: Vec<String> = valid
        .into_iter()
        .chain(pairs.into_iter().filter(|p| !p.starts_with(&prefix)))
        .collect();
    headers.remove(header::COOKIE);
    if let Ok(value) = axum::http::HeaderValue::from_str(&merged.join("; ")) {
        headers.insert(header::COOKIE, value);
    }
}

#[cfg(test)]
mod dedupe_tests {
    use axum::http::{HeaderMap, HeaderValue, header};
    use axum::response::IntoResponse;
    use axum_extra::extract::cookie::{Cookie, Key, SignedCookieJar};

    use super::{dedupe_session_cookie, session_cookie};

    fn signed_pair(key: &Key, value: &str) -> String {
        let response = SignedCookieJar::new(key.clone())
            .add(Cookie::new(session_cookie(), value.to_string()))
            .into_response();
        let set = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_string();
        set.split(';').next().unwrap().to_string()
    }

    #[test]
    fn a_stale_session_cookie_does_not_shadow_a_valid_one() {
        let key = Key::generate();
        let stale = signed_pair(&Key::generate(), "old-install");
        let fresh = signed_pair(&key, "fresh");
        let mut headers = HeaderMap::new();
        let raw = format!("{stale}; vv_flash=x; {fresh}");
        headers.insert(header::COOKIE, HeaderValue::from_str(&raw).unwrap());

        dedupe_session_cookie(&mut headers, &key);

        let jar = SignedCookieJar::from_headers(&headers, key);
        assert_eq!(
            jar.get(session_cookie())
                .map(|c| c.value().to_string())
                .as_deref(),
            Some("fresh")
        );
        assert!(
            headers[header::COOKIE]
                .to_str()
                .unwrap()
                .contains("vv_flash=x")
        );
    }

    #[test]
    fn a_single_session_cookie_is_left_alone() {
        let key = Key::generate();
        let raw = format!("{}; other=1", signed_pair(&key, "only"));
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_str(&raw).unwrap());
        dedupe_session_cookie(&mut headers, &key);
        assert_eq!(headers[header::COOKIE].to_str().unwrap(), raw);
    }
}
