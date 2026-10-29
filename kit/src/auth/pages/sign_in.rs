//! Setup wizard, sign-in (password, then TOTP or a recovery code) and
//! sign-out.

use std::time::Instant;

use askama::Template;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;

use crate::auth::guard::{self, login_jar, secure_cookies};
use crate::auth::session::{self, PendingLogin};
use crate::auth::{self, totp};
use crate::db::settings as db_settings;
use crate::db::users::{self as db_users, User};
use crate::http::{ApiError, ApiResult, HtmlTemplate};
use crate::i18n::Tr as _;
use crate::page::{PageCtx, flash};
use crate::state::Core;

use crate::auth::guard::PeerAddr;

/// The site name (TOTP issuer); the product's name when none is set.
pub(super) async fn site_name(core: &Core) -> String {
    core.with_conn(|c| db_settings::get(c, db_settings::SITE_NAME))
        .await
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| core.cfg.product.to_string())
}

pub(super) fn wait_message(wait: std::time::Duration) -> String {
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
    /// The setup token the visitor came with (carried in a hidden field).
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
            site_name: crate::config::get().product.to_string(),
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

/// The page shown instead of the form when the setup token is missing or
/// wrong, with a field to enter it (`429` when guesses come too fast).
fn setup_locked(throttled: Option<std::time::Duration>) -> Response {
    let (status, error) = match throttled {
        Some(wait) => (StatusCode::TOO_MANY_REQUESTS, wait_message(wait)),
        None => (
            StatusCode::FORBIDDEN,
            crate::i18n::t("auth.setup_token_required").into(),
        ),
    };
    let page = SetupTemplate {
        error: Some(error),
        locked: true,
        ..SetupTemplate::blank()
    };
    (status, HtmlTemplate(page)).into_response()
}

/// Why [`check_setup_token`] turned a visitor away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupTokenError {
    /// A setup token is required and none was given.
    Missing,
    /// The token given is wrong.
    Wrong,
    /// Too many wrong tokens from this address; try again after the wait.
    Throttled(std::time::Duration),
}

/// The throttle key for setup tokens tried by a visitor whose address is
/// not known.
const SETUP_UNKNOWN_KEY: &str = "setup:unknown";

/// Checks the visitor's setup token. `Ok` when none is required
/// ([`session::required_setup_token`]) or `given` matches. A token that is
/// tried and wrong counts against the client address in the sign-in IP
/// limiter (key `setup:<ip>`), so it can't be guessed online; being without
/// a token does not. Without an address (`peer` is `None` and no trusted
/// forwarded header) the attempts share the key `setup:unknown`: they are
/// throttled all the same, and only together.
///
/// The kit's own setup page uses it; a product with
/// [`CredentialMode::External`](crate::config::CredentialMode::External)
/// calls it from its own setup page, before it creates the first admin.
pub fn check_setup_token(
    core: &Core,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
    given: Option<&str>,
) -> Result<(), SetupTokenError> {
    let Some(expected) = session::required_setup_token() else {
        return Ok(());
    };
    let given = given.map(str::trim).filter(|g| !g.is_empty());
    let Some(given) = given else {
        tracing::warn!("setup page opened without the setup token");
        return Err(SetupTokenError::Missing);
    };
    // Never unthrottled: when there is no client address (a product that
    // passed `peer = None`, and no trusted forwarded header), all such
    // visitors share one key.
    let key = match session::client_ip(headers, peer, session::trust_proxy()) {
        Some(ip) => format!("setup:{ip}"),
        None => {
            tracing::warn!(
                "setup token checked without a client address; throttled under one shared key"
            );
            SETUP_UNKNOWN_KEY.to_string()
        }
    };
    if let Err(wait) = core.limits.ip.reserve(&key, Instant::now()) {
        tracing::warn!(ip = ?key, "setup token attempts throttled");
        return Err(SetupTokenError::Throttled(wait));
    }
    if !session::setup_token_matches(Some(&expected), Some(given)) {
        tracing::warn!(ip = ?key, "setup attempted with a wrong setup token");
        return Err(SetupTokenError::Wrong);
    }
    core.limits.ip.clear(&key);
    Ok(())
}

/// [`check_setup_token`], with the locked page as the error.
fn gate_setup_token(
    core: &Core,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
    given: Option<&str>,
) -> Result<(), Box<Response>> {
    check_setup_token(core, headers, peer, given).map_err(|e| {
        Box::new(match e {
            SetupTokenError::Throttled(wait) => setup_locked(Some(wait)),
            SetupTokenError::Missing | SetupTokenError::Wrong => setup_locked(None),
        })
    })
}

/// Creates the first admin with a password hash the product made itself and
/// stores the site name and time zone, in one transaction. The kit never
/// looks inside the hash: for an account that signs in some other way it can
/// be a marker no password verifies against, such as `"!external"`. `None`
/// when a user already exists, so two racing setups can't both succeed.
/// Check [`check_setup_token`] and the user name
/// ([`username_problem`](crate::auth::username_problem)) first.
pub async fn insert_first_admin(
    core: &Core,
    username: &str,
    password_hash: &str,
    site_name: &str,
    timezone: &str,
) -> ApiResult<Option<i64>> {
    let (username, password_hash) = (username.to_string(), password_hash.to_string());
    let (site_name, timezone) = (site_name.to_string(), timezone.to_string());
    core.with_conn(move |c| {
        let tx = c.transaction()?;
        let id = db_users::insert_first_admin(&tx, &username, &password_hash, "")?;
        if id.is_some() {
            db_settings::set(&tx, db_settings::SITE_NAME, &site_name)?;
            db_settings::set(&tx, db_settings::TIMEZONE, &timezone)?;
            tx.commit()?;
        }
        Ok(id)
    })
    .await
}

pub(super) async fn setup_form(
    State(core): State<Core>,
    headers: HeaderMap,
    PeerAddr(peer): PeerAddr,
    Query(query): Query<SetupQuery>,
) -> ApiResult<Response> {
    if guard::has_users(&core).await? {
        return Ok(Redirect::to("/login").into_response());
    }
    if let Err(locked) = gate_setup_token(&core, &headers, peer, query.token.as_deref()) {
        return Ok(*locked);
    }
    let page = SetupTemplate {
        token: query.token.unwrap_or_default().trim().to_string(),
        ..SetupTemplate::blank()
    };
    Ok(HtmlTemplate(page).into_response())
}

pub(super) async fn setup_submit(
    State(core): State<Core>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    PeerAddr(peer): PeerAddr,
    axum::Form(form): axum::Form<SetupForm>,
) -> ApiResult<Response> {
    if guard::has_users(&core).await? {
        return Ok(Redirect::to("/login").into_response());
    }
    if let Err(locked) = gate_setup_token(&core, &headers, peer, Some(&form.token)) {
        return Ok(*locked);
    }
    // Only the token (the locked page's field): on to the form itself.
    if session::required_setup_token().is_some()
        && form.username.is_empty()
        && form.password.is_empty()
        && form.confirm.is_empty()
    {
        let page = SetupTemplate {
            token: form.token.trim().to_string(),
            ..SetupTemplate::blank()
        };
        return Ok(HtmlTemplate(page).into_response());
    }

    let username = form.username.trim().to_string();
    let site = form.site_name.trim().to_string();
    let timezone = form.timezone.trim().to_string();
    let mut page = SetupTemplate {
        username: username.clone(),
        site_name: site.clone(),
        timezone: if crate::tz::valid_timezone(&timezone) {
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
        crate::config::get().product.to_string()
    } else {
        site
    };
    let tz = page.timezone.clone();
    let created = insert_first_admin(&core, &username, &password_hash, &site_name, &tz).await?;
    let Some(user_id) = created else {
        // Someone else finished setup first.
        return Ok(Redirect::to("/login").into_response());
    };
    let user = core
        .with_conn(move |c| db_users::get_user(c, user_id))
        .await?
        .ok_or(ApiError::NotFound)?;
    core.with_conn(move |c| db_users::touch_last_login(c, user_id))
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
    /// The public URL's origin, when passkeys can work there: the page
    /// offers them (`static/passkeys.js` shows the button only on that
    /// origin).
    passkey_origin: Option<String>,
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
    passkey_origin: Option<String>,
) -> Response {
    let page = LoginTemplate {
        title: crate::i18n::t("login.title").into(),
        error,
        notice: None,
        username,
        next,
        passkey_origin,
    };
    (status, HtmlTemplate(page)).into_response()
}

pub(super) async fn login_form(
    State(core): State<Core>,
    jar: SignedCookieJar,
    Query(query): Query<NextQuery>,
) -> ApiResult<Response> {
    if !guard::has_users(&core).await? {
        return Ok(Redirect::to("/setup").into_response());
    }
    let next = session::sanitize_next(query.next.as_deref());
    if guard::session_user(&core, &jar).await?.is_some() {
        return Ok(Redirect::to(&next).into_response());
    }
    // Messages from the previous step (timed-out 2FA, password changed, …).
    let (ctx, jar) = PageCtx::build(&core, None, &jar, crate::i18n::t("login.title"), "", "").await;
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
        passkey_origin: super::passkeys::relying_party(&core)
            .await
            .map(|rp| rp.origin),
    };
    Ok((jar, HtmlTemplate(page)).into_response())
}

pub(super) async fn login_submit(
    State(core): State<Core>,
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
    let blocked = match core.limits.account.reserve(&user_key, now) {
        Err(wait) => Some(wait),
        Ok(()) => match ip_key.as_deref().map(|k| core.limits.ip.reserve(k, now)) {
            Some(Err(wait)) => {
                core.limits.account.release(&user_key);
                Some(wait)
            }
            _ => None,
        },
    };
    let passkey_origin = super::passkeys::relying_party(&core)
        .await
        .map(|rp| rp.origin);
    if let Some(wait) = blocked {
        tracing::warn!(user = %username, ip = ?ip_key, "login throttled");
        return Ok(login_page(
            StatusCode::TOO_MANY_REQUESTS,
            Some(wait_message(wait)),
            username,
            next,
            passkey_origin,
        ));
    }

    let lookup = username.clone();
    let user = if username.is_empty() || form.password.len() > auth::MAX_PASSWORD_LEN {
        None
    } else {
        core.with_conn(move |c| db_users::get_user_by_username(c, &lookup))
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
            passkey_origin,
        ));
    };

    core.limits.account.clear(&user_key);
    if let Some(k) = &ip_key {
        core.limits.ip.release(k);
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
    finish_login(&core, jar, &user, &next, secure).await
}

async fn finish_login(
    core: &Core,
    jar: SignedCookieJar,
    user: &User,
    next: &str,
    secure: bool,
) -> ApiResult<Response> {
    let id = user.id;
    core.with_conn(move |c| db_users::touch_last_login(c, id))
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
    core: &Core,
    jar: &SignedCookieJar,
) -> ApiResult<Option<(PendingLogin, User)>> {
    let Some(pending) = jar
        .get(crate::config::get().pending_2fa_cookie)
        .and_then(|c| PendingLogin::decode(c.value()))
        .filter(|p| p.is_live(session::now_unix()))
    else {
        return Ok(None);
    };
    let id = pending.user_id;
    let user = core.with_conn(move |c| db_users::get_user(c, id)).await?;
    Ok(user
        .filter(|u| u.active && u.totp_enabled && u.session_version == pending.version)
        .map(|u| (pending, u)))
}

fn back_to_login(jar: SignedCookieJar, message: &str) -> Response {
    let jar = jar.remove(session::removal_cookie(
        crate::config::get().pending_2fa_cookie,
    ));
    let jar = flash(jar, "error", message);
    (jar, Redirect::to("/login")).into_response()
}

pub(super) async fn two_factor_form(
    State(core): State<Core>,
    jar: SignedCookieJar,
) -> ApiResult<Response> {
    if pending_login(&core, &jar).await?.is_none() {
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
    core: &Core,
    user_id: i64,
    account: &str,
    code: &str,
) -> ApiResult<bool> {
    Ok(!matches!(
        check_second_factor(core, user_id, account, code).await?,
        SecondFactor::Invalid
    ))
}

async fn check_second_factor(
    core: &Core,
    user_id: i64,
    account: &str,
    code: &str,
) -> ApiResult<SecondFactor> {
    let Some(tf) = core
        .with_conn(move |c| db_users::get_totp(c, user_id))
        .await?
    else {
        return Ok(SecondFactor::Invalid);
    };
    let Some(secret) = tf.secret.filter(|_| tf.enabled) else {
        return Ok(SecondFactor::Invalid);
    };
    // Sealed at rest; a secret that can't be opened never matches (recovery
    // codes still do).
    let secret = totp::open_secret(&core.session_key, &secret);

    if totp::normalize_code(code).is_some() {
        let Some(secret) = secret else {
            return Ok(SecondFactor::Invalid);
        };
        let issuer = site_name(core).await;
        let totp = totp::build(&secret, account, &issuer)?;
        let now = u64::try_from(session::now_unix()).unwrap_or(0);
        let Some(step) = totp::match_step(&totp, code, now, tf.last_step) else {
            return Ok(SecondFactor::Invalid);
        };
        let step = i64::try_from(step).map_err(ApiError::internal)?;
        let fresh = core
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
        let consumed = core
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

pub(super) async fn two_factor_submit(
    State(core): State<Core>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    axum::Form(form): axum::Form<TwoFactorForm>,
) -> ApiResult<Response> {
    let Some((pending, user)) = pending_login(&core, &jar).await? else {
        return Ok(back_to_login(jar, crate::i18n::t("auth.timed_out")));
    };
    let key = format!("2fa:{}", user.id);
    let now = Instant::now();
    // Counted up front, like the password step.
    if let Err(wait) = core.limits.account.reserve(&key, now) {
        return Ok(two_factor_page(
            StatusCode::TOO_MANY_REQUESTS,
            Some(wait_message(wait)),
        ));
    }

    match check_second_factor(&core, user.id, &user.username, form.code.trim()).await? {
        SecondFactor::Invalid => {
            tracing::info!(user = %user.username, "failed second factor");
            Ok(two_factor_page(
                StatusCode::UNAUTHORIZED,
                Some(crate::i18n::t("auth.code_failed").into()),
            ))
        }
        outcome => {
            core.limits.account.clear(&key);
            let jar = match outcome {
                SecondFactor::Recovery { remaining } => flash(
                    jar,
                    "info",
                    crate::i18n::tf("auth.recovery_used", &[("n", &remaining.to_string())]),
                ),
                _ => jar,
            };
            finish_login(&core, jar, &user, &pending.next, secure_cookies(&headers)).await
        }
    }
}

// ---------------------------------------------------------------------------
// Logout
// ---------------------------------------------------------------------------

pub(super) async fn logout(jar: SignedCookieJar) -> impl IntoResponse {
    let jar = jar
        .remove(session::removal_cookie(crate::config::get().session_cookie))
        .remove(session::removal_cookie(
            crate::config::get().pending_2fa_cookie,
        ));
    // A fresh confirmation ends with the session it was given for.
    let jar = match crate::config::get().features.credentials {
        crate::config::CredentialMode::Plain => jar,
        crate::config::CredentialMode::External(_) => auth::reauth::clear(jar),
    };
    (jar, Redirect::to("/login"))
}

#[cfg(test)]
mod tests {
    use askama::Template;

    use super::{LoginTemplate, SetupTemplate};
    use crate::auth::Role;
    use crate::i18n::Tr as _;
    use crate::page::{PageCtx, UserView};

    #[derive(Template)]
    #[template(
        source = "{% extends \"layout.html\" %}{% block content %}page{% endblock %}",
        ext = "html"
    )]
    struct AppPage {
        ctx: PageCtx,
    }

    #[derive(Template)]
    #[template(
        source = "{% extends \"public_layout.html\" %}{% block content %}status{% endblock %}",
        ext = "html"
    )]
    struct PublicPage {
        page_title: String,
        theme: String,
        custom_css: Option<String>,
    }

    fn ctx(user: Option<UserView>) -> PageCtx {
        PageCtx {
            title: "Dashboard".into(),
            nav: "",
            subnav: "",
            user,
            site_name: "Vigilant Vole".into(),
            flash: None,
            lang: "en".into(),
            dir: "ltr",
        }
    }

    /// The product and version show on signed-in pages only: they would
    /// tell an attacker which version to aim at.
    #[test]
    fn the_version_footer_is_for_signed_in_pages_only() {
        let cfg = crate::config::get();
        let version = cfg.version;
        let user = UserView {
            id: 1,
            username: "admin".into(),
            display_name: "admin".into(),
            role: Role::Admin,
            initials: "A".into(),
        };
        let signed_in = AppPage {
            ctx: ctx(Some(user)),
        }
        .render()
        .unwrap();
        let notes = cfg.release_notes_url().expect("test config has releases");
        assert!(
            notes.ends_with(&format!("/releases/tag/v{version}")),
            "{notes}"
        );
        let footer = format!(
            "<footer class=\"app-foot container\">\n    {} <a href=\"{notes}\">{version}</a>\n  </footer>",
            cfg.product
        );
        assert!(
            signed_in.replace("\r\n", "\n").contains(&footer),
            "{signed_in}"
        );

        let anonymous = AppPage { ctx: ctx(None) }.render().unwrap();
        let public = PublicPage {
            page_title: "Status".into(),
            theme: "auto".into(),
            custom_css: None,
        }
        .render()
        .unwrap();
        let login = LoginTemplate {
            title: "Log in".into(),
            error: None,
            notice: None,
            username: String::new(),
            next: "/".into(),
            passkey_origin: None,
        }
        .render()
        .unwrap();
        let setup = SetupTemplate::blank().render().unwrap();
        for (page, html) in [
            ("layout without a user", anonymous),
            ("public page", public),
            ("sign-in", login),
            ("setup", setup),
        ] {
            assert!(!html.contains("app-foot"), "{page}: {html}");
            assert!(!html.contains(version), "{page} shows the version: {html}");
            assert!(!html.contains("/releases/"), "{page}: {html}");
        }
    }
}
