//! Login / 2FA / setup / logout pages and the auth + CSRF middleware.
//! Owned by the auth agent.
//!
//! Public helpers for other modules:
//! - [`session_user`]: the logged-in user on a public page (cookie only)
//! - [`authenticate_token`]: resolve an API token to a (scope-capped) user
//! - [`authenticate_headers`]: same, reading `Authorization: Bearer|Basic`

use crate::i18n::Tr as _;
use std::time::Instant;

use askama::Template;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;

use super::{ApiError, ApiResult, AppState, HtmlTemplate, flash};
use crate::auth::session::{self, PENDING_2FA_COOKIE, PendingLogin};
#[cfg(test)]
use crate::auth::tokens;
use crate::auth::{self, AuthMethod, CurrentUser, SESSION_COOKIE, rate_limit, totp};
#[cfg(test)]
use crate::db::api_tokens as db_tokens;
use crate::db::settings as db_settings;
use crate::db::users::{self as db_users, User};
use crate::kit::auth::guard;
pub use crate::kit::auth::guard::{PeerAddr, csrf_guard, login_jar, require_auth, secure_cookies};
use crate::kit::state::Core;

/// Protected auth routes: none (2FA management lives under `/settings/security`).
pub fn router() -> Router<AppState> {
    Router::new()
}

pub fn public_router() -> Router<AppState> {
    Router::new()
        .route("/setup", get(setup_form).post(setup_submit))
        .route("/login", get(login_form).post(login_submit))
        .route("/login/2fa", get(two_factor_form).post(two_factor_submit))
        .route("/logout", post(logout))
}

// ---------------------------------------------------------------------------
// Small extractors / helpers
// ---------------------------------------------------------------------------

fn core(state: &AppState) -> Core {
    axum::extract::FromRef::from_ref(state)
}

async fn has_users(state: &AppState) -> ApiResult<bool> {
    guard::has_users(&core(state)).await
}

/// Resolves the session cookie to an active user, if any. Protected routes get
/// this via the `CurrentUser` extractor; public pages (e.g. a status page that
/// shows an "Edit" link to logged-in editors) can call it directly.
pub async fn session_user(
    state: &AppState,
    jar: &SignedCookieJar,
) -> ApiResult<Option<CurrentUser>> {
    guard::session_user(&core(state), jar).await
}

pub async fn authenticate_token_detailed(
    state: &AppState,
    token: &str,
) -> ApiResult<Option<(CurrentUser, AuthMethod)>> {
    guard::authenticate_token_detailed(&core(state), token).await
}

/// Resolves an API token to the user it acts as, with the role capped by
/// the token's scope (read → viewer, write → the user's role). `None` for
/// unknown, malformed, expired or revoked tokens and disabled users. Records
/// `last_used_at` (at most once a minute).
pub async fn authenticate_token(state: &AppState, token: &str) -> ApiResult<Option<CurrentUser>> {
    guard::authenticate_token(&core(state), token).await
}

/// [`authenticate_token`] for `Authorization: Bearer <token>` or
/// `Authorization: Basic base64(any:<token>)` (Prometheus `basic_auth`).
/// `None` when the header is missing or the token is invalid.
pub async fn authenticate_headers(
    state: &AppState,
    headers: &HeaderMap,
) -> ApiResult<Option<CurrentUser>> {
    guard::authenticate_headers(&core(state), headers).await
}

async fn site_name(state: &AppState) -> String {
    state
        .settings()
        .await
        .map(|s| s.site_name)
        .unwrap_or_else(|_| db_settings::Settings::default().site_name)
}

fn wait_message(wait: std::time::Duration) -> String {
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

// ---------------------------------------------------------------------------
// Setup wizard
// ---------------------------------------------------------------------------

/// `auth_layout.html` contract: `title` + `error`.
#[derive(Template)]
#[template(path = "setup.html")]
struct SetupTemplate {
    title: String,
    error: Option<String>,
    username: String,
    site_name: String,
    timezone: String,
    username_error: String,
    password_error: String,
    confirm_error: String,
    site_name_error: String,
    /// The `VV_SETUP_TOKEN` the visitor came with (carried in a hidden field).
    token: String,
    /// A setup token is required and missing/wrong: no form.
    locked: bool,
}

impl SetupTemplate {
    fn blank() -> Self {
        Self {
            title: crate::i18n::t("auth.welcome").into(),
            error: None,
            token: String::new(),
            locked: false,
            username: String::new(),
            site_name: db_settings::Settings::default().site_name,
            timezone: "UTC".into(),
            username_error: String::new(),
            password_error: String::new(),
            confirm_error: String::new(),
            site_name_error: String::new(),
        }
    }

    fn has_errors(&self) -> bool {
        [
            &self.username_error,
            &self.password_error,
            &self.confirm_error,
            &self.site_name_error,
        ]
        .iter()
        .any(|e| !e.is_empty())
    }
}

#[derive(Deserialize)]
pub struct SetupForm {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    confirm: String,
    #[serde(default)]
    site_name: String,
    #[serde(default)]
    timezone: String,
    #[serde(default)]
    token: String,
}

#[derive(Deserialize, Default)]
pub struct SetupQuery {
    token: Option<String>,
}

/// The page shown instead of the form when `VV_SETUP_TOKEN` doesn't match.
fn setup_locked() -> Response {
    let page = SetupTemplate {
        error: Some(crate::i18n::t("auth.setup_token_required").into()),
        locked: true,
        ..SetupTemplate::blank()
    };
    (StatusCode::FORBIDDEN, HtmlTemplate(page)).into_response()
}

fn setup_unlocked(token: Option<&str>) -> bool {
    session::setup_token_matches(session::SETUP_TOKEN.as_deref(), token)
}

async fn setup_form(
    State(state): State<AppState>,
    Query(query): Query<SetupQuery>,
) -> ApiResult<Response> {
    if has_users(&state).await? {
        return Ok(Redirect::to("/login").into_response());
    }
    if !setup_unlocked(query.token.as_deref()) {
        tracing::warn!("setup page opened without the VV_SETUP_TOKEN");
        return Ok(setup_locked());
    }
    let page = SetupTemplate {
        token: query.token.unwrap_or_default().trim().to_string(),
        ..SetupTemplate::blank()
    };
    Ok(HtmlTemplate(page).into_response())
}

async fn setup_submit(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    axum::Form(form): axum::Form<SetupForm>,
) -> ApiResult<Response> {
    if has_users(&state).await? {
        return Ok(Redirect::to("/login").into_response());
    }
    if !setup_unlocked(Some(&form.token)) {
        tracing::warn!("setup attempted without the VV_SETUP_TOKEN");
        return Ok(setup_locked());
    }

    let username = form.username.trim().to_string();
    let site = form.site_name.trim().to_string();
    let timezone = form.timezone.trim().to_string();
    let mut page = SetupTemplate {
        username: username.clone(),
        site_name: site.clone(),
        timezone: if super::settings::valid_timezone(&timezone) {
            timezone.clone()
        } else {
            "UTC".into()
        },
        token: form.token.trim().to_string(),
        ..SetupTemplate::blank()
    };
    if let Some(problem) = auth::username_problem(&username) {
        page.username_error = problem.into();
    }
    if let Some(problem) = auth::password_problem(&form.password, &username) {
        page.password_error = problem.into();
    } else if form.password != form.confirm {
        page.confirm_error = crate::i18n::t("auth.passwords_mismatch").into();
    }
    if site.chars().count() > 80 {
        page.site_name_error = crate::i18n::t("general.err_site_name").into();
    }
    if page.has_errors() {
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, HtmlTemplate(page)).into_response());
    }

    let password_hash = auth::hash_password_async(form.password).await?;
    let site_name = if site.is_empty() {
        db_settings::Settings::default().site_name
    } else {
        site
    };
    let tz = page.timezone.clone();
    let created = state
        .with_conn(move |c| {
            let tx = c.transaction()?;
            let id = db_users::insert_first_admin(&tx, &username, &password_hash, "")?;
            if id.is_some() {
                db_settings::set(&tx, db_settings::SITE_NAME, &site_name)?;
                db_settings::set(&tx, db_settings::TIMEZONE, &tz)?;
                tx.commit()?;
            }
            Ok(id)
        })
        .await?;
    let Some(user_id) = created else {
        // Someone else finished setup first.
        return Ok(Redirect::to("/login").into_response());
    };
    let user = state
        .with_conn(move |c| db_users::get_user(c, user_id))
        .await?
        .ok_or(ApiError::NotFound)?;
    state
        .with_conn(move |c| db_users::touch_last_login(c, user_id))
        .await?;

    tracing::info!(user = %user.username, "setup complete; first admin created");
    let jar = login_jar(jar, user.id, user.session_version, secure_cookies(&headers));
    let jar = flash(jar, "success", crate::i18n::t("auth.all_set"));
    Ok((jar, Redirect::to("/")).into_response())
}

// ---------------------------------------------------------------------------
// Login (step 1: password)
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    title: String,
    error: Option<String>,
    /// Informational message (e.g. "Your password was changed").
    notice: Option<String>,
    username: String,
    next: String,
}

#[derive(Deserialize, Default)]
pub struct NextQuery {
    next: Option<String>,
}

#[derive(Deserialize)]
pub struct LoginForm {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    next: Option<String>,
}

fn login_page(
    status: StatusCode,
    error: Option<String>,
    username: String,
    next: String,
) -> Response {
    let page = LoginTemplate {
        title: crate::i18n::t("login.title").into(),
        error,
        notice: None,
        username,
        next,
    };
    (status, HtmlTemplate(page)).into_response()
}

async fn login_form(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    Query(query): Query<NextQuery>,
) -> ApiResult<Response> {
    if !has_users(&state).await? {
        return Ok(Redirect::to("/setup").into_response());
    }
    let next = session::sanitize_next(query.next.as_deref());
    if session_user(&state, &jar).await?.is_some() {
        return Ok(Redirect::to(&next).into_response());
    }
    // Messages from the previous step (timed-out 2FA, password changed, …).
    let (ctx, jar) =
        super::PageCtx::build(&state, None, &jar, crate::i18n::t("login.title"), "", "").await;
    let (error, notice) = match ctx.flash {
        Some(f) if f.kind == "error" => (Some(f.message), None),
        Some(f) => (None, Some(f.message)),
        None => (None, None),
    };
    let page = LoginTemplate {
        title: crate::i18n::t("login.title").into(),
        error,
        notice,
        username: String::new(),
        next,
    };
    Ok((jar, HtmlTemplate(page)).into_response())
}

async fn login_submit(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    PeerAddr(peer): PeerAddr,
    axum::Form(form): axum::Form<LoginForm>,
) -> ApiResult<Response> {
    let username = form.username.trim().to_string();
    let next = session::sanitize_next(form.next.as_deref());
    let ip_key =
        session::client_ip(&headers, peer, session::trust_proxy()).map(|ip| format!("ip:{ip}"));
    let user_key = format!("user:{}", username.to_lowercase());
    let now = Instant::now();

    // Count the attempt before the (slow) Argon2 check, so a parallel burst
    // can't get more guesses in than the budget allows. Success undoes it.
    let blocked = match rate_limit::ACCOUNT.reserve(&user_key, now) {
        Err(wait) => Some(wait),
        Ok(()) => match ip_key.as_deref().map(|k| rate_limit::IP.reserve(k, now)) {
            Some(Err(wait)) => {
                rate_limit::ACCOUNT.release(&user_key);
                Some(wait)
            }
            _ => None,
        },
    };
    if let Some(wait) = blocked {
        tracing::warn!(user = %username, ip = ?ip_key, "login throttled");
        return Ok(login_page(
            StatusCode::TOO_MANY_REQUESTS,
            Some(wait_message(wait)),
            username,
            next,
        ));
    }

    let lookup = username.clone();
    let user = if username.is_empty() || form.password.len() > auth::MAX_PASSWORD_LEN {
        None
    } else {
        state
            .with_conn(move |c| db_users::get_user_by_username(c, &lookup))
            .await?
    };
    // Always run one Argon2 verification, against a dummy hash when there is
    // no usable account, so timing doesn't reveal which usernames exist.
    let (hash, candidate) = match user {
        Some(u) if u.active => (u.password_hash.clone(), Some(u)),
        _ => (auth::DUMMY_HASH.clone(), None),
    };
    let password_ok = auth::verify_password_async(hash, form.password).await?;
    let Some(user) = candidate.filter(|_| password_ok) else {
        // Already counted by `reserve`.
        tracing::info!(user = %username, ip = ?ip_key, "failed login");
        return Ok(login_page(
            StatusCode::UNAUTHORIZED,
            Some(crate::i18n::t("auth.login_failed").into()),
            username,
            next,
        ));
    };

    rate_limit::ACCOUNT.clear(&user_key);
    if let Some(k) = &ip_key {
        rate_limit::IP.release(k);
    }
    let secure = secure_cookies(&headers);
    if user.totp_enabled {
        let pending = PendingLogin {
            user_id: user.id,
            version: user.session_version,
            issued_at: session::now_unix(),
            next,
        };
        let jar = jar.add(session::pending_cookie(&pending, secure));
        return Ok((jar, Redirect::to("/login/2fa")).into_response());
    }
    finish_login(&state, jar, &user, &next, secure).await
}

async fn finish_login(
    state: &AppState,
    jar: SignedCookieJar,
    user: &User,
    next: &str,
    secure: bool,
) -> ApiResult<Response> {
    let id = user.id;
    state
        .with_conn(move |c| db_users::touch_last_login(c, id))
        .await?;
    tracing::info!(user = %user.username, "login");
    let jar = login_jar(jar, user.id, user.session_version, secure);
    Ok((jar, Redirect::to(next)).into_response())
}

// ---------------------------------------------------------------------------
// Login (step 2: TOTP or recovery code)
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "login_2fa.html")]
struct TwoFactorTemplate {
    title: String,
    error: Option<String>,
}

#[derive(Deserialize)]
pub struct TwoFactorForm {
    #[serde(default)]
    code: String,
}

fn two_factor_page(status: StatusCode, error: Option<String>) -> Response {
    let page = TwoFactorTemplate {
        title: crate::i18n::t("login.two_factor_title").into(),
        error,
    };
    (status, HtmlTemplate(page)).into_response()
}

/// The pending login, if its cookie is valid, fresh and still matches the user.
async fn pending_login(
    state: &AppState,
    jar: &SignedCookieJar,
) -> ApiResult<Option<(PendingLogin, User)>> {
    let Some(pending) = jar
        .get(PENDING_2FA_COOKIE)
        .and_then(|c| PendingLogin::decode(c.value()))
        .filter(|p| p.is_live(session::now_unix()))
    else {
        return Ok(None);
    };
    let id = pending.user_id;
    let user = state.with_conn(move |c| db_users::get_user(c, id)).await?;
    Ok(user
        .filter(|u| u.active && u.totp_enabled && u.session_version == pending.version)
        .map(|u| (pending, u)))
}

fn back_to_login(jar: SignedCookieJar, message: &str) -> Response {
    let jar = jar.remove(session::removal_cookie(PENDING_2FA_COOKIE));
    let jar = flash(jar, "error", message);
    (jar, Redirect::to("/login")).into_response()
}

async fn two_factor_form(
    State(state): State<AppState>,
    jar: SignedCookieJar,
) -> ApiResult<Response> {
    if pending_login(&state, &jar).await?.is_none() {
        return Ok(back_to_login(jar, crate::i18n::t("auth.timed_out")));
    }
    Ok(two_factor_page(StatusCode::OK, None))
}

/// Outcome of checking a second factor.
enum SecondFactor {
    Totp,
    Recovery { remaining: usize },
    Invalid,
}

/// Verifies a TOTP or recovery code for `user_id` and consumes it (single use).
pub async fn verify_second_factor(
    state: &AppState,
    user_id: i64,
    account: &str,
    code: &str,
) -> ApiResult<bool> {
    Ok(!matches!(
        check_second_factor(state, user_id, account, code).await?,
        SecondFactor::Invalid
    ))
}

async fn check_second_factor(
    state: &AppState,
    user_id: i64,
    account: &str,
    code: &str,
) -> ApiResult<SecondFactor> {
    let Some(tf) = state
        .with_conn(move |c| db_users::get_totp(c, user_id))
        .await?
    else {
        return Ok(SecondFactor::Invalid);
    };
    let Some(secret) = tf.secret.filter(|_| tf.enabled) else {
        return Ok(SecondFactor::Invalid);
    };

    if totp::normalize_code(code).is_some() {
        let issuer = site_name(state).await;
        let totp = totp::build(&secret, account, &issuer)?;
        let now = u64::try_from(session::now_unix()).unwrap_or(0);
        let Some(step) = totp::match_step(&totp, code, now, tf.last_step) else {
            return Ok(SecondFactor::Invalid);
        };
        let step = i64::try_from(step).map_err(ApiError::internal)?;
        let fresh = state
            .with_conn(move |c| db_users::consume_totp_step(c, user_id, step))
            .await?;
        return Ok(if fresh {
            SecondFactor::Totp
        } else {
            SecondFactor::Invalid
        });
    }

    if totp::looks_like_recovery_code(code) {
        let hashes = tf.recovery_codes.clone();
        let input = code.to_string();
        let found = tokio::task::spawn_blocking(move || totp::find_recovery_code(&hashes, &input))
            .await
            .map_err(ApiError::internal)?;
        let Some(used) = found else {
            return Ok(SecondFactor::Invalid);
        };
        let before = tf.recovery_codes;
        let remaining = before.len().saturating_sub(1);
        let consumed = state
            .with_conn(move |c| db_users::consume_recovery_code(c, user_id, &before, &used))
            .await?;
        return Ok(if consumed {
            SecondFactor::Recovery { remaining }
        } else {
            SecondFactor::Invalid
        });
    }
    Ok(SecondFactor::Invalid)
}

async fn two_factor_submit(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    axum::Form(form): axum::Form<TwoFactorForm>,
) -> ApiResult<Response> {
    let Some((pending, user)) = pending_login(&state, &jar).await? else {
        return Ok(back_to_login(jar, crate::i18n::t("auth.timed_out")));
    };
    let key = format!("2fa:{}", user.id);
    let now = Instant::now();
    // Counted up front, like the password step.
    if let Err(wait) = rate_limit::ACCOUNT.reserve(&key, now) {
        return Ok(two_factor_page(
            StatusCode::TOO_MANY_REQUESTS,
            Some(wait_message(wait)),
        ));
    }

    match check_second_factor(&state, user.id, &user.username, form.code.trim()).await? {
        SecondFactor::Invalid => {
            tracing::info!(user = %user.username, "failed second factor");
            Ok(two_factor_page(
                StatusCode::UNAUTHORIZED,
                Some(crate::i18n::t("auth.code_failed").into()),
            ))
        }
        outcome => {
            rate_limit::ACCOUNT.clear(&key);
            let jar = match outcome {
                SecondFactor::Recovery { remaining } => flash(
                    jar,
                    "info",
                    crate::i18n::tf("auth.recovery_used", &[("n", &remaining.to_string())]),
                ),
                _ => jar,
            };
            finish_login(&state, jar, &user, &pending.next, secure_cookies(&headers)).await
        }
    }
}

// ---------------------------------------------------------------------------
// Logout
// ---------------------------------------------------------------------------

async fn logout(jar: SignedCookieJar) -> impl IntoResponse {
    let jar = jar
        .remove(session::removal_cookie(SESSION_COOKIE))
        .remove(session::removal_cookie(PENDING_2FA_COOKIE));
    (jar, Redirect::to("/login"))
}

// ---------------------------------------------------------------------------
// Middleware + request authentication
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
