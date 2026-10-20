//! General settings (admin) and the personal Security page (everyone).
//! Owned by the auth agent.
//!
//! Routes:
//! - `GET  /settings` → `/settings/general` (admins) or `/settings/security`
//! - `GET|POST /settings/general`
//! - `GET  /settings/security`
//! - `POST /settings/security/profile` · `/password` · `/sessions`
//! - `POST /settings/security/2fa/setup` · `/2fa/cancel` · `/2fa/enable` · `/2fa/disable` · `/2fa/recovery`

use crate::i18n::Tr as _;
use std::time::Instant;

use askama::Template;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;

use super::auth::{login_jar, secure_cookies, verify_second_factor};
use super::{ApiError, ApiResult, AppState, HtmlTemplate, PageCtx, flash};
use crate::auth::{self, CurrentUser, rate_limit, totp};
use crate::db::settings as db_settings;
use crate::db::users as db_users;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/settings", get(settings_home))
        .route("/settings/general", get(general_page).post(general_save))
        .route("/settings/security", get(security_page))
        .route("/settings/security/profile", post(save_profile))
        .route("/settings/security/password", post(change_password))
        .route("/settings/security/sessions", post(sign_out_others))
        .route("/settings/security/2fa/setup", post(totp_setup))
        .route("/settings/security/2fa/cancel", post(totp_cancel))
        .route("/settings/security/2fa/enable", post(totp_enable))
        .route("/settings/security/2fa/disable", post(totp_disable))
        .route("/settings/security/2fa/recovery", post(totp_new_recovery_codes))
}

async fn settings_home(user: CurrentUser) -> Redirect {
    if user.is_admin() {
        Redirect::to("/settings/general")
    } else {
        Redirect::to("/settings/security")
    }
}

// ---------------------------------------------------------------------------
// Time zones & languages (also used by the setup wizard)
// ---------------------------------------------------------------------------


/// Used when the system has no tz database (e.g. a scratch container).
const FALLBACK_ZONES: &[&str] = &[
    "UTC", "Africa/Cairo", "Africa/Johannesburg", "Africa/Lagos", "Africa/Nairobi", "America/Anchorage",
    "America/Argentina/Buenos_Aires", "America/Bogota", "America/Chicago", "America/Denver", "America/Halifax",
    "America/Los_Angeles", "America/Mexico_City", "America/New_York", "America/Phoenix", "America/Sao_Paulo",
    "America/Toronto", "America/Vancouver", "Asia/Bangkok", "Asia/Dubai", "Asia/Hong_Kong", "Asia/Jakarta",
    "Asia/Jerusalem", "Asia/Karachi", "Asia/Kolkata", "Asia/Manila", "Asia/Seoul", "Asia/Shanghai",
    "Asia/Singapore", "Asia/Tehran", "Asia/Tokyo", "Atlantic/Reykjavik", "Australia/Adelaide",
    "Australia/Brisbane", "Australia/Perth", "Australia/Sydney", "Europe/Amsterdam", "Europe/Athens",
    "Europe/Berlin", "Europe/Brussels", "Europe/Copenhagen", "Europe/Dublin", "Europe/Helsinki",
    "Europe/Istanbul", "Europe/Kyiv", "Europe/Lisbon", "Europe/London", "Europe/Madrid", "Europe/Moscow",
    "Europe/Oslo", "Europe/Paris", "Europe/Prague", "Europe/Rome", "Europe/Stockholm", "Europe/Vienna",
    "Europe/Warsaw", "Europe/Zurich", "Pacific/Auckland", "Pacific/Honolulu",
];

fn is_listed_zone(name: &str) -> bool {
    name == "UTC"
        || (name.contains('/')
            && !name.starts_with("posix")
            && !name.starts_with("right/")
            && !name.starts_with("Etc/")
            && !name.starts_with("SystemV/"))
}

/// IANA zone names for the picker (sorted, `UTC` first).
pub fn timezones() -> Vec<String> {
    let mut zones: Vec<String> = jiff::tz::db()
        .available()
        .map(|n| n.as_str().to_string())
        .filter(|n| is_listed_zone(n))
        .collect();
    if zones.len() < 10 {
        zones = FALLBACK_ZONES.iter().map(|z| (*z).to_string()).collect();
    }
    zones.sort();
    zones.dedup();
    zones.retain(|z| z != "UTC");
    zones.insert(0, "UTC".into());
    zones
}

/// Is `name` a time zone we can use?
pub fn valid_timezone(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    name == "UTC" || jiff::tz::TimeZone::get(name).is_ok() || FALLBACK_ZONES.contains(&name)
}

/// Validates and normalises a public base URL: `http(s)://host[:port][/path]`,
/// no credentials, query or fragment; trailing slash removed. Empty → `None`.
pub fn normalize_base_url(input: &str) -> Result<Option<String>, &'static str> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    let hint = crate::i18n::t("general.err_url");
    let url = url::Url::parse(input).map_err(|_| hint)?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
        return Err(hint);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(crate::i18n::t("general.err_url_credentials"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(crate::i18n::t("general.err_url_query"));
    }
    Ok(Some(url.as_str().trim_end_matches('/').to_string()))
}

// ---------------------------------------------------------------------------
// General (admin)
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "settings_general.html")]
struct GeneralTemplate {
    ctx: PageCtx,
    site_name: String,
    base_url: String,
    timezone: String,
    language: String,
    retention_days: String,
    badges_public: bool,
    agent_confirmation_default: bool,
    timezones: Vec<String>,
    languages: Vec<(&'static str, String)>,
    db_size: String,
    heartbeat_count: String,
    site_name_error: String,
    base_url_error: String,
    timezone_error: String,
    retention_error: String,
}

impl GeneralTemplate {
    fn is_language(&self, code: &str) -> bool {
        self.language == code
    }

    fn has_errors(&self) -> bool {
        [&self.site_name_error, &self.base_url_error, &self.timezone_error, &self.retention_error]
            .iter()
            .any(|e| !e.is_empty())
    }
}

#[derive(Deserialize)]
pub struct GeneralForm {
    #[serde(default)]
    site_name: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    timezone: String,
    #[serde(default)]
    language: String,
    #[serde(default)]
    retention_days: String,
    #[serde(default)]
    badges_public: bool,
    #[serde(default)]
    agent_confirmation_default: bool,
}

/// Database file size and heartbeat count for the retention help text.
async fn db_stats(state: &AppState) -> ApiResult<(String, String)> {
    let (bytes, beats) = state
        .with_conn(|c| {
            let pages: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0))?;
            let size: i64 = c.query_row("PRAGMA page_size", [], |r| r.get(0))?;
            let beats: i64 = c.query_row("SELECT COUNT(*) FROM heartbeats", [], |r| r.get(0))?;
            Ok((pages.saturating_mul(size), beats))
        })
        .await?;
    Ok((format_bytes(bytes), format_count(beats)))
}

pub fn format_bytes(bytes: i64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} {}", crate::i18n::t("general.bytes"));
    }
    let mut value = bytes as f64;
    let mut unit = crate::i18n::t("general.bytes");
    for u in UNITS {
        if value < 1000.0 {
            break;
        }
        value /= 1000.0;
        unit = u;
    }
    let decimals = if value < 10.0 { 1 } else { 0 };
    format!("{} {unit}", crate::i18n::fmt_dec(value, decimals))
}

/// 1234567 → "1,234,567" ("1 234 567" in Bokmål).
pub fn format_count(n: i64) -> String {
    crate::i18n::fmt_int(n)
}

async fn general_template(state: &AppState, ctx: PageCtx, s: &db_settings::Settings) -> ApiResult<GeneralTemplate> {
    let (db_size, heartbeat_count) = db_stats(state).await?;
    Ok(GeneralTemplate {
        ctx,
        site_name: s.site_name.clone(),
        base_url: s.base_url.clone().unwrap_or_default(),
        timezone: s.timezone.clone(),
        language: s.language.clone(),
        retention_days: s.retention_days.to_string(),
        badges_public: s.badges_public,
        agent_confirmation_default: s.agent_confirmation_default,
        timezones: timezones(),
        languages: crate::i18n::language_choices(),
        db_size,
        heartbeat_count,
        site_name_error: String::new(),
        base_url_error: String::new(),
        timezone_error: String::new(),
        retention_error: String::new(),
    })
}

async fn general_page(State(state): State<AppState>, user: CurrentUser, jar: SignedCookieJar) -> ApiResult<Response> {
    user.require_admin()?;
    let settings = state.settings().await?;
    let (ctx, jar) = PageCtx::build(&state, Some(&user), &jar, crate::i18n::t("nav.general"), "settings", "general").await;
    let page = general_template(&state, ctx, &settings).await?;
    Ok((jar, HtmlTemplate(page)).into_response())
}

async fn general_save(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Form(form): Form<GeneralForm>,
) -> ApiResult<Response> {
    user.require_admin()?;
    let current = state.settings().await?;
    let (ctx, jar) = PageCtx::build(&state, Some(&user), &jar, crate::i18n::t("nav.general"), "settings", "general").await;
    let mut page = general_template(&state, ctx, &current).await?;

    let site_name = form.site_name.trim().to_string();
    page.site_name = site_name.clone();
    if site_name.chars().count() > 80 {
        page.site_name_error = crate::i18n::t("general.err_site_name").into();
    }
    page.base_url = form.base_url.trim().to_string();
    let base_url = match normalize_base_url(&form.base_url) {
        Ok(url) => url,
        Err(msg) => {
            page.base_url_error = msg.into();
            None
        }
    };
    let timezone = form.timezone.trim().to_string();
    page.timezone = timezone.clone();
    if !valid_timezone(&timezone) {
        page.timezone_error = crate::i18n::t("general.err_timezone").into();
    }
    let language = if crate::i18n::Lang::from_exact_code(&form.language).is_some() {
        form.language.clone()
    } else {
        "en".to_string()
    };
    page.language = language.clone();
    page.retention_days = form.retention_days.trim().to_string();
    let retention = form.retention_days.trim().parse::<i64>().ok().filter(|d| (1..=3650).contains(d));
    if retention.is_none() {
        page.retention_error = crate::i18n::t("general.err_retention").into();
    }
    page.badges_public = form.badges_public;
    page.agent_confirmation_default = form.agent_confirmation_default;
    if page.has_errors() {
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, jar, HtmlTemplate(page)).into_response());
    }

    let site_name = if site_name.is_empty() { db_settings::Settings::default().site_name } else { site_name };
    let retention = retention.unwrap_or(current.retention_days);
    let badges = form.badges_public;
    let confirm_default = form.agent_confirmation_default;
    let new_lang = crate::i18n::Lang::from_code(&language).unwrap_or_default();
    let uid = user.id;
    let own_lang = state
        .with_conn(move |c| {
            let tx = c.transaction()?;
            db_settings::set(&tx, db_settings::SITE_NAME, &site_name)?;
            db_settings::set_opt(&tx, db_settings::BASE_URL, base_url.as_deref())?;
            db_settings::set(&tx, db_settings::TIMEZONE, &timezone)?;
            db_settings::set(&tx, db_settings::LANGUAGE, &language)?;
            db_settings::set(&tx, db_settings::RETENTION_DAYS, &retention.to_string())?;
            db_settings::set_bool(&tx, db_settings::BADGES_PUBLIC, badges)?;
            db_settings::set_bool(&tx, db_settings::AGENT_CONFIRMATION_DEFAULT, confirm_default)?;
            tx.commit()?;
            Ok(db_users::get_user(c, uid)?.and_then(|u| u.language))
        })
        .await?;
    tracing::info!(user = %user.username, "general settings saved");
    // Confirm in the language the next page will use.
    let lang = own_lang.as_deref().and_then(crate::i18n::Lang::from_code).unwrap_or(new_lang);
    let jar = flash(jar, "success", crate::i18n::t_in(lang, "general.saved"));
    Ok((jar, Redirect::to("/settings/general")).into_response())
}

// ---------------------------------------------------------------------------
// Security (every user, for themselves)
// ---------------------------------------------------------------------------

/// 2FA enrolment in progress: what the user scans / types.
struct Enrolment {
    qr: String,
    secret_grouped: String,
}

#[derive(Template)]
#[template(path = "settings_security.html")]
struct SecurityTemplate {
    ctx: PageCtx,
    username: String,
    display_name: String,
    last_login_at: Option<String>,
    totp_enabled: bool,
    recovery_remaining: usize,
    enrolment: Option<Enrolment>,
    /// Plaintext recovery codes, only right after they were generated.
    new_codes: Vec<String>,
    new_codes_text: String,
    /// The user's own language code ("" = site default).
    language: String,
    languages: Vec<(&'static str, String)>,
    /// Native name of the site default language.
    site_language_name: &'static str,
}

async fn security_template(state: &AppState, user: &CurrentUser, ctx: PageCtx, new_codes: Vec<String>) -> ApiResult<SecurityTemplate> {
    let id = user.id;
    let (record, tf) = state
        .with_conn(move |c| Ok((db_users::get_user(c, id)?, db_users::get_totp(c, id)?)))
        .await?;
    let record = record.ok_or(ApiError::Unauthorized)?;
    let tf = tf.unwrap_or_default();
    let enrolment = match (&tf.secret, tf.enabled) {
        (Some(secret), false) => {
            let issuer = state.settings().await?.site_name;
            let t = totp::build(secret, &record.username, &issuer)?;
            Some(Enrolment {
                qr: totp::qr_data_uri(&totp::otpauth_url(&t)?)?,
                secret_grouped: totp::format_secret(secret),
            })
        }
        _ => None,
    };
    Ok(SecurityTemplate {
        ctx,
        username: record.username.clone(),
        display_name: record.display_name.clone(),
        last_login_at: record.last_login_at.clone(),
        totp_enabled: tf.enabled,
        recovery_remaining: tf.recovery_codes.len(),
        enrolment,
        new_codes_text: new_codes.join("\n"),
        new_codes,
        language: record.language.clone().unwrap_or_default(),
        languages: crate::i18n::language_choices(),
        site_language_name: crate::i18n::site_language(state).await.unwrap_or_default().native_name(),
    })
}

async fn render_security(state: &AppState, user: &CurrentUser, jar: SignedCookieJar, new_codes: Vec<String>) -> ApiResult<Response> {
    let (ctx, jar) = PageCtx::build(state, Some(user), &jar, crate::i18n::t("nav.security"), "settings", "security").await;
    let page = security_template(state, user, ctx, new_codes).await?;
    Ok((jar, HtmlTemplate(page)).into_response())
}

async fn security_page(State(state): State<AppState>, user: CurrentUser, jar: SignedCookieJar) -> ApiResult<Response> {
    render_security(&state, &user, jar, Vec::new()).await
}

fn back_to_security(jar: SignedCookieJar, kind: &str, message: &str) -> Response {
    (flash(jar, kind, message), Redirect::to("/settings/security")).into_response()
}

/// Re-authentication for sensitive changes, throttled per user so a hijacked
/// session can't be used to brute-force the password.
async fn confirm_password(state: &AppState, user: &CurrentUser, password: String) -> ApiResult<Result<(), String>> {
    let key = format!("reauth:{}", user.id);
    let now = Instant::now();
    if let Some(wait) = rate_limit::ACCOUNT.retry_after(&key, now) {
        let secs = wait.as_secs().max(1).to_string();
        return Ok(Err(crate::i18n::tf("sec.too_many_secs", &[("n", &secs)])));
    }
    let id = user.id;
    let record = state
        .with_conn(move |c| db_users::get_user(c, id))
        .await?
        .ok_or(ApiError::Unauthorized)?;
    if password.len() <= auth::MAX_PASSWORD_LEN && auth::verify_password_async(record.password_hash, password).await? {
        rate_limit::ACCOUNT.clear(&key);
        Ok(Ok(()))
    } else {
        rate_limit::ACCOUNT.record_failure(&key, now);
        Ok(Err(crate::i18n::t("sec.wrong_password").into()))
    }
}

#[derive(Deserialize)]
pub struct ProfileForm {
    #[serde(default)]
    display_name: String,
    /// "" = follow the site default.
    #[serde(default)]
    language: String,
}

async fn save_profile(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Form(form): Form<ProfileForm>,
) -> ApiResult<Response> {
    let name = form.display_name.trim().to_string();
    if name.chars().count() > 80 {
        return Ok(back_to_security(jar, "error", crate::i18n::t("sec.name_long")));
    }
    let language = form.language.trim().to_string();
    if !language.is_empty() && crate::i18n::Lang::from_exact_code(&language).is_none() {
        return Ok(back_to_security(jar, "error", crate::i18n::t("sec.bad_language")));
    }
    let id = user.id;
    let stored = (!language.is_empty()).then_some(language);
    let chosen = stored.as_deref().and_then(crate::i18n::Lang::from_code);
    state
        .with_conn(move |c| {
            db_users::set_display_name(c, id, &name)?;
            db_users::set_language(c, id, stored.as_deref())
        })
        .await?;
    // Confirm in the language the next page will use.
    let lang = match chosen {
        Some(lang) => lang,
        None => crate::i18n::site_language(&state).await.unwrap_or_default(),
    };
    Ok(back_to_security(jar, "success", crate::i18n::t_in(lang, "sec.profile_saved")))
}

#[derive(Deserialize)]
pub struct PasswordForm {
    #[serde(default)]
    current_password: String,
    #[serde(default)]
    new_password: String,
    #[serde(default)]
    confirm_password: String,
}

async fn change_password(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Form(form): Form<PasswordForm>,
) -> ApiResult<Response> {
    if let Err(msg) = confirm_password(&state, &user, form.current_password).await? {
        return Ok(back_to_security(jar, "error", &msg));
    }
    if let Some(problem) = auth::password_problem(&form.new_password, &user.username) {
        return Ok(back_to_security(jar, "error", problem));
    }
    if form.new_password != form.confirm_password {
        return Ok(back_to_security(jar, "error", crate::i18n::t("sec.passwords_mismatch")));
    }
    let hash = auth::hash_password_async(form.new_password).await?;
    let id = user.id;
    let version = state.with_conn(move |c| db_users::set_password(c, id, &hash)).await?;
    tracing::info!(user = %user.username, "password changed; other sessions signed out");
    // Keep this browser signed in with the new session version.
    let jar = login_jar(jar, id, version, secure_cookies(&headers));
    Ok(back_to_security(jar, "success", crate::i18n::t("sec.password_changed")))
}

async fn sign_out_others(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let id = user.id;
    let version = state.with_conn(move |c| db_users::bump_session_version(c, id)).await?;
    tracing::info!(user = %user.username, "signed out other sessions");
    let jar = login_jar(jar, id, version, secure_cookies(&headers));
    Ok(back_to_security(jar, "success", crate::i18n::t("sec.signed_out_others")))
}

async fn totp_setup(State(state): State<AppState>, user: CurrentUser, jar: SignedCookieJar) -> ApiResult<Response> {
    let id = user.id;
    let secret = totp::generate_secret();
    let started = state
        .with_conn(move |c| db_users::set_pending_totp_secret(c, id, &secret))
        .await?;
    if !started {
        return Ok(back_to_security(jar, "info", crate::i18n::t("sec.tfa_already_on")));
    }
    Ok(Redirect::to("/settings/security#two-factor").into_response())
}

async fn totp_cancel(State(state): State<AppState>, user: CurrentUser, jar: SignedCookieJar) -> ApiResult<Response> {
    let id = user.id;
    let tf = state.with_conn(move |c| db_users::get_totp(c, id)).await?.unwrap_or_default();
    if !tf.enabled {
        state.with_conn(move |c| db_users::disable_totp(c, id)).await?;
    }
    Ok(back_to_security(jar, "info", crate::i18n::t("sec.tfa_cancelled")))
}

#[derive(Deserialize)]
pub struct CodeForm {
    #[serde(default)]
    code: String,
}

async fn totp_enable(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Form(form): Form<CodeForm>,
) -> ApiResult<Response> {
    let id = user.id;
    let tf = state.with_conn(move |c| db_users::get_totp(c, id)).await?.unwrap_or_default();
    let Some(secret) = tf.secret.filter(|_| !tf.enabled) else {
        return Ok(back_to_security(jar, "info", crate::i18n::t("sec.tfa_start_first")));
    };
    let key = format!("2fa-setup:{id}");
    let now = Instant::now();
    if rate_limit::ACCOUNT.retry_after(&key, now).is_some() {
        return Ok(back_to_security(jar, "error", crate::i18n::t("sec.too_many_wait")));
    }
    let issuer = state.settings().await?.site_name;
    let t = totp::build(&secret, &user.username, &issuer)?;
    let now_unix = u64::try_from(crate::auth::session::now_unix()).unwrap_or(0);
    let Some(step) = totp::match_step(&t, &form.code, now_unix, None) else {
        rate_limit::ACCOUNT.record_failure(&key, now);
        return Ok(back_to_security(
            jar,
            "error",
            crate::i18n::t("sec.code_mismatch"),
        ));
    };
    rate_limit::ACCOUNT.clear(&key);
    let step = i64::try_from(step).map_err(ApiError::internal)?;

    let codes = totp::generate_recovery_codes();
    let to_hash = codes.clone();
    let hashes = tokio::task::spawn_blocking(move || totp::hash_recovery_codes(&to_hash))
        .await
        .map_err(ApiError::internal)??;
    let enabled = state
        .with_conn(move |c| db_users::enable_totp(c, id, step, &hashes))
        .await?;
    if !enabled {
        return Ok(back_to_security(jar, "error", crate::i18n::t("sec.tfa_changed")));
    }
    tracing::info!(user = %user.username, "two-factor authentication enabled");
    let jar = flash(jar, "success", crate::i18n::t("sec.tfa_on"));
    render_security(&state, &user, jar, codes).await
}

#[derive(Deserialize)]
pub struct DisableForm {
    #[serde(default)]
    password: String,
    #[serde(default)]
    code: String,
}

async fn totp_disable(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Form(form): Form<DisableForm>,
) -> ApiResult<Response> {
    if let Err(msg) = confirm_password(&state, &user, form.password).await? {
        return Ok(back_to_security(jar, "error", &msg));
    }
    let key = format!("2fa:{}", user.id);
    let now = Instant::now();
    if rate_limit::ACCOUNT.retry_after(&key, now).is_some() {
        return Ok(back_to_security(jar, "error", crate::i18n::t("sec.too_many_wait")));
    }
    if !verify_second_factor(&state, user.id, &user.username, form.code.trim()).await? {
        rate_limit::ACCOUNT.record_failure(&key, now);
        return Ok(back_to_security(jar, "error", crate::i18n::t("sec.code_invalid")));
    }
    rate_limit::ACCOUNT.clear(&key);
    let id = user.id;
    state.with_conn(move |c| db_users::disable_totp(c, id)).await?;
    tracing::info!(user = %user.username, "two-factor authentication disabled");
    Ok(back_to_security(jar, "success", crate::i18n::t("sec.tfa_off")))
}

#[derive(Deserialize)]
pub struct PasswordOnlyForm {
    #[serde(default)]
    password: String,
}

async fn totp_new_recovery_codes(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Form(form): Form<PasswordOnlyForm>,
) -> ApiResult<Response> {
    if let Err(msg) = confirm_password(&state, &user, form.password).await? {
        return Ok(back_to_security(jar, "error", &msg));
    }
    let id = user.id;
    let tf = state.with_conn(move |c| db_users::get_totp(c, id)).await?.unwrap_or_default();
    if !tf.enabled {
        return Ok(back_to_security(jar, "info", crate::i18n::t("sec.tfa_turn_on_first")));
    }
    let codes = totp::generate_recovery_codes();
    let to_hash = codes.clone();
    let hashes = tokio::task::spawn_blocking(move || totp::hash_recovery_codes(&to_hash))
        .await
        .map_err(ApiError::internal)??;
    state
        .with_conn(move |c| db_users::replace_recovery_codes(c, id, &hashes))
        .await?;
    tracing::info!(user = %user.username, "recovery codes regenerated");
    let jar = flash(jar, "success", crate::i18n::t("sec.codes_created"));
    render_security(&state, &user, jar, codes).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_is_normalised() {
        assert_eq!(normalize_base_url(" https://up.example.com/ "), Ok(Some("https://up.example.com".into())));
        assert_eq!(normalize_base_url("http://10.0.0.5:3001/vole/"), Ok(Some("http://10.0.0.5:3001/vole".into())));
        assert_eq!(normalize_base_url(""), Ok(None));
    }

    #[test]
    fn base_url_rejects_bad_input() {
        for bad in ["up.example.com", "ftp://x.example", "https://u:p@x.example", "https://x.example/?a=1", "javascript:alert(1)"] {
            assert!(normalize_base_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn timezone_validation() {
        assert!(valid_timezone("UTC"));
        assert!(valid_timezone("Europe/Oslo"));
        assert!(!valid_timezone("Mars/Olympus_Mons"));
        assert!(!valid_timezone(""));
    }

    #[test]
    fn timezone_list_starts_with_utc() {
        let zones = timezones();
        assert_eq!(zones.first().map(String::as_str), Some("UTC"));
        assert!(zones.iter().any(|z| z == "Europe/Oslo"));
    }

    #[test]
    fn bytes_are_human_readable() {
        assert_eq!(format_bytes(512), "512 bytes");
        assert_eq!(format_bytes(1_500), "1.5 KB");
        assert_eq!(format_bytes(23_400_000), "23 MB");
    }

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(1_234_567), "1,234,567");
        assert_eq!(format_count(999), "999");
    }
}
