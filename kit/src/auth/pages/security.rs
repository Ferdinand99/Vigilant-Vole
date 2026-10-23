//! The personal Security page (every user, for themselves):
//! - `GET  /settings/security`
//! - `POST /settings/security/profile` · `/password` · `/sessions`
//! - `POST /settings/security/2fa/setup` · `/2fa/cancel` · `/2fa/enable` · `/2fa/disable` · `/2fa/recovery`
//! - passkeys: see [`super::passkeys`]

use std::time::Instant;

use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;

use crate::auth::guard::{login_jar, secure_cookies};
use crate::auth::{self, CurrentUser, totp};
use crate::db::passkeys as db_passkeys;
use crate::db::users as db_users;
use crate::http::{ApiError, ApiResult, HtmlTemplate};
use crate::i18n::Tr as _;
use crate::page::{PageCtx, flash};
use crate::state::Core;

/// One of the user's passkeys.
struct PasskeyRow {
    id: i64,
    name: String,
    created_at: String,
    last_used_at: Option<String>,
}

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
    passkeys: Vec<PasskeyRow>,
    /// The public URL's origin, when passkeys can work there.
    passkey_origin: Option<String>,
}

async fn security_template(
    core: &Core,
    user: &CurrentUser,
    ctx: PageCtx,
    new_codes: Vec<String>,
) -> ApiResult<SecurityTemplate> {
    let id = user.id;
    let (record, tf, passkeys) = core
        .with_conn(move |c| {
            Ok((
                db_users::get_user(c, id)?,
                db_users::get_totp(c, id)?,
                db_passkeys::list_for_user(c, id)?,
            ))
        })
        .await?;
    let record = record.ok_or(ApiError::Unauthorized)?;
    let tf = tf.unwrap_or_default();
    let enrolment = match (&tf.secret, tf.enabled) {
        (Some(secret), false) => {
            let issuer = super::sign_in::site_name(core).await;
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
        site_language_name: crate::i18n::site_language(core)
            .await
            .unwrap_or_default()
            .native_name(),
        passkeys: passkeys
            .into_iter()
            .map(|p| PasskeyRow {
                id: p.id,
                name: p.name,
                created_at: p.created_at,
                last_used_at: p.last_used_at,
            })
            .collect(),
        passkey_origin: super::passkeys::relying_party(core)
            .await
            .map(|rp| rp.origin),
    })
}

async fn render_security(
    core: &Core,
    user: &CurrentUser,
    jar: SignedCookieJar,
    new_codes: Vec<String>,
) -> ApiResult<Response> {
    let (ctx, jar) = PageCtx::build(
        core,
        Some(user),
        &jar,
        crate::i18n::t("nav.security"),
        "settings",
        "security",
    )
    .await;
    let page = security_template(core, user, ctx, new_codes).await?;
    Ok((jar, HtmlTemplate(page)).into_response())
}

pub(super) async fn security_page(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<Response> {
    render_security(&core, &user, jar, Vec::new()).await
}

fn back_to_security(jar: SignedCookieJar, kind: &str, message: &str) -> Response {
    (
        flash(jar, kind, message),
        Redirect::to("/settings/security"),
    )
        .into_response()
}

/// Re-authentication for sensitive changes, throttled per user so a hijacked
/// session can't be used to brute-force the password.
pub(super) async fn confirm_password(
    core: &Core,
    user: &CurrentUser,
    password: String,
) -> ApiResult<Result<(), String>> {
    let key = format!("reauth:{}", user.id);
    let now = Instant::now();
    if let Some(wait) = core.limits.account.retry_after(&key, now) {
        let secs = wait.as_secs().max(1).to_string();
        return Ok(Err(crate::i18n::tf("sec.too_many_secs", &[("n", &secs)])));
    }
    let id = user.id;
    let record = core
        .with_conn(move |c| db_users::get_user(c, id))
        .await?
        .ok_or(ApiError::Unauthorized)?;
    if password.len() <= auth::MAX_PASSWORD_LEN
        && auth::verify_password_async(record.password_hash, password).await?
    {
        core.limits.account.clear(&key);
        Ok(Ok(()))
    } else {
        core.limits.account.record_failure(&key, now);
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

pub(super) async fn save_profile(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Form(form): Form<ProfileForm>,
) -> ApiResult<Response> {
    let name = form.display_name.trim().to_string();
    if name.chars().count() > 80 {
        return Ok(back_to_security(
            jar,
            "error",
            crate::i18n::t("sec.name_long"),
        ));
    }
    let language = form.language.trim().to_string();
    if !language.is_empty() && crate::i18n::Lang::from_exact_code(&language).is_none() {
        return Ok(back_to_security(
            jar,
            "error",
            crate::i18n::t("sec.bad_language"),
        ));
    }
    let id = user.id;
    let stored = (!language.is_empty()).then_some(language);
    let chosen = stored.as_deref().and_then(crate::i18n::Lang::from_code);
    core.with_conn(move |c| {
        db_users::set_display_name(c, id, &name)?;
        db_users::set_language(c, id, stored.as_deref())
    })
    .await?;
    // Confirm in the language the next page will use.
    let lang = match chosen {
        Some(lang) => lang,
        None => crate::i18n::site_language(&core).await.unwrap_or_default(),
    };
    Ok(back_to_security(
        jar,
        "success",
        crate::i18n::t_in(lang, "sec.profile_saved"),
    ))
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

pub(super) async fn change_password(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Form(form): Form<PasswordForm>,
) -> ApiResult<Response> {
    if let Err(msg) = confirm_password(&core, &user, form.current_password).await? {
        return Ok(back_to_security(jar, "error", &msg));
    }
    if let Some(problem) = auth::password_problem(&form.new_password, &user.username) {
        return Ok(back_to_security(jar, "error", problem));
    }
    if form.new_password != form.confirm_password {
        return Ok(back_to_security(
            jar,
            "error",
            crate::i18n::t("sec.passwords_mismatch"),
        ));
    }
    let hash = auth::hash_password_async(form.new_password).await?;
    let id = user.id;
    let version = core
        .with_conn(move |c| db_users::set_password(c, id, &hash))
        .await?;
    tracing::info!(user = %user.username, "password changed; other sessions signed out");
    // Keep this browser signed in with the new session version.
    let jar = login_jar(jar, id, version, secure_cookies(&headers));
    Ok(back_to_security(
        jar,
        "success",
        crate::i18n::t("sec.password_changed"),
    ))
}

pub(super) async fn sign_out_others(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let id = user.id;
    let version = core
        .with_conn(move |c| db_users::bump_session_version(c, id))
        .await?;
    tracing::info!(user = %user.username, "signed out other sessions");
    let jar = login_jar(jar, id, version, secure_cookies(&headers));
    Ok(back_to_security(
        jar,
        "success",
        crate::i18n::t("sec.signed_out_others"),
    ))
}

pub(super) async fn totp_setup(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<Response> {
    let id = user.id;
    let secret = totp::generate_secret();
    let started = core
        .with_conn(move |c| db_users::set_pending_totp_secret(c, id, &secret))
        .await?;
    if !started {
        return Ok(back_to_security(
            jar,
            "info",
            crate::i18n::t("sec.tfa_already_on"),
        ));
    }
    Ok(Redirect::to("/settings/security#two-factor").into_response())
}

pub(super) async fn totp_cancel(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<Response> {
    let id = user.id;
    let tf = core
        .with_conn(move |c| db_users::get_totp(c, id))
        .await?
        .unwrap_or_default();
    if !tf.enabled {
        core.with_conn(move |c| db_users::disable_totp(c, id))
            .await?;
    }
    Ok(back_to_security(
        jar,
        "info",
        crate::i18n::t("sec.tfa_cancelled"),
    ))
}

#[derive(Deserialize)]
pub struct CodeForm {
    #[serde(default)]
    code: String,
}

pub(super) async fn totp_enable(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Form(form): Form<CodeForm>,
) -> ApiResult<Response> {
    let id = user.id;
    let tf = core
        .with_conn(move |c| db_users::get_totp(c, id))
        .await?
        .unwrap_or_default();
    let Some(secret) = tf.secret.filter(|_| !tf.enabled) else {
        return Ok(back_to_security(
            jar,
            "info",
            crate::i18n::t("sec.tfa_start_first"),
        ));
    };
    let key = format!("2fa-setup:{id}");
    let now = Instant::now();
    if core.limits.account.retry_after(&key, now).is_some() {
        return Ok(back_to_security(
            jar,
            "error",
            crate::i18n::t("sec.too_many_wait"),
        ));
    }
    let issuer = super::sign_in::site_name(&core).await;
    let t = totp::build(&secret, &user.username, &issuer)?;
    let now_unix = u64::try_from(crate::auth::session::now_unix()).unwrap_or(0);
    let Some(step) = totp::match_step(&t, &form.code, now_unix, None) else {
        core.limits.account.record_failure(&key, now);
        return Ok(back_to_security(
            jar,
            "error",
            crate::i18n::t("sec.code_mismatch"),
        ));
    };
    core.limits.account.clear(&key);
    let step = i64::try_from(step).map_err(ApiError::internal)?;

    let codes = totp::generate_recovery_codes();
    let to_hash = codes.clone();
    let hashes = tokio::task::spawn_blocking(move || totp::hash_recovery_codes(&to_hash))
        .await
        .map_err(ApiError::internal)??;
    let enabled = core
        .with_conn(move |c| db_users::enable_totp(c, id, step, &hashes))
        .await?;
    if !enabled {
        return Ok(back_to_security(
            jar,
            "error",
            crate::i18n::t("sec.tfa_changed"),
        ));
    }
    tracing::info!(user = %user.username, "two-factor authentication enabled");
    let jar = flash(jar, "success", crate::i18n::t("sec.tfa_on"));
    render_security(&core, &user, jar, codes).await
}

#[derive(Deserialize)]
pub struct DisableForm {
    #[serde(default)]
    password: String,
    #[serde(default)]
    code: String,
}

pub(super) async fn totp_disable(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Form(form): Form<DisableForm>,
) -> ApiResult<Response> {
    if let Err(msg) = confirm_password(&core, &user, form.password).await? {
        return Ok(back_to_security(jar, "error", &msg));
    }
    let key = format!("2fa:{}", user.id);
    let now = Instant::now();
    if core.limits.account.retry_after(&key, now).is_some() {
        return Ok(back_to_security(
            jar,
            "error",
            crate::i18n::t("sec.too_many_wait"),
        ));
    }
    if !super::sign_in::verify_second_factor(&core, user.id, &user.username, form.code.trim())
        .await?
    {
        core.limits.account.record_failure(&key, now);
        return Ok(back_to_security(
            jar,
            "error",
            crate::i18n::t("sec.code_invalid"),
        ));
    }
    core.limits.account.clear(&key);
    let id = user.id;
    core.with_conn(move |c| db_users::disable_totp(c, id))
        .await?;
    tracing::info!(user = %user.username, "two-factor authentication disabled");
    Ok(back_to_security(
        jar,
        "success",
        crate::i18n::t("sec.tfa_off"),
    ))
}

#[derive(Deserialize)]
pub struct PasswordOnlyForm {
    #[serde(default)]
    password: String,
}

pub(super) async fn totp_new_recovery_codes(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Form(form): Form<PasswordOnlyForm>,
) -> ApiResult<Response> {
    if let Err(msg) = confirm_password(&core, &user, form.password).await? {
        return Ok(back_to_security(jar, "error", &msg));
    }
    let id = user.id;
    let tf = core
        .with_conn(move |c| db_users::get_totp(c, id))
        .await?
        .unwrap_or_default();
    if !tf.enabled {
        return Ok(back_to_security(
            jar,
            "info",
            crate::i18n::t("sec.tfa_turn_on_first"),
        ));
    }
    let codes = totp::generate_recovery_codes();
    let to_hash = codes.clone();
    let hashes = tokio::task::spawn_blocking(move || totp::hash_recovery_codes(&to_hash))
        .await
        .map_err(ApiError::internal)??;
    core.with_conn(move |c| db_users::replace_recovery_codes(c, id, &hashes))
        .await?;
    tracing::info!(user = %user.username, "recovery codes regenerated");
    let jar = flash(jar, "success", crate::i18n::t("sec.codes_created"));
    render_security(&core, &user, jar, codes).await
}
