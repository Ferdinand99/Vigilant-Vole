//! One-time sign-in codes ([`auth::sign_in_codes`](crate::auth::sign_in_codes)):
//! - `POST /settings/users/{id}/sign-in-code` (admin): creates a code for
//!   another user and shows it once, and emails it when the admin ticked
//!   "Also send by email" (offered when the instance has mail settings and
//!   the user an address)
//! - `POST /settings/users/{id}/sign-in-code/revoke` (admin): ends the
//!   user's pending code
//! - `GET /sign-in-code`, `POST /sign-in-code` (public): the user types
//!   their user name, the code and a new password and is signed in. With
//!   `CredentialMode::External` the GET leads to the product's password page
//!   (`?code=1`) and the POST is gone.
//!
//! Every way the code can fail (expired, used, revoked, unknown, someone
//! else's, no such user) gets the same answer ([`failed`]). The code is
//! never logged, and neither is the user name a visitor typed (it could be a
//! code pasted into the wrong field, or hold a line break): the log has
//! [`log_ref`](sign_in_codes::log_ref) of it.
//!
//! The email is made from the translated templates, with fixed fields: the
//! subject has only the site name, and the code goes into the body last,
//! after every other value is in, so no name can pull it into the subject.

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, REFERRER_POLICY};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;

use crate::auth::guard::{PeerAddr, login_jar, secure_cookies};
use crate::auth::session::{self, PendingLogin};
use crate::auth::sign_in_codes::{self, Refused};
use crate::auth::{self, CurrentUser, reauth};
use crate::db::sign_in_codes as db_codes;
use crate::db::users as db_users;
use crate::http::{ApiError, ApiResult, HtmlTemplate};
use crate::i18n::{Lang, Tr as _};
use crate::page::flash;
use crate::state::Core;

// ---------------------------------------------------------------------------
// Admin
// ---------------------------------------------------------------------------

/// A code just created, for the Users page to show once.
pub(super) struct NewCode {
    pub username: String,
    /// As shown: eight groups of four.
    pub code: String,
    /// Where people enter it.
    pub url: String,
    /// The address it was emailed to.
    pub emailed_to: Option<String>,
    /// An email was due and could not be sent.
    pub mail_failed: bool,
}

#[derive(Deserialize)]
pub struct ResetForm {
    /// The admin's own password (`CredentialMode::Plain`).
    #[serde(default)]
    current_password: String,
    /// Also email the code ("Also send by email"; a form without the field
    /// sends nothing).
    #[serde(default)]
    send_email: bool,
}

fn back(jar: SignedCookieJar, kind: &str, message: impl Into<String>) -> Response {
    (flash(jar, kind, message), Redirect::to("/settings/users")).into_response()
}

/// A value for a mail: without braces (so that no name can spell a
/// placeholder) and without control characters (so that no name can break a
/// header or a line).
fn plain(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '{' | '}'))
        .collect()
}

/// The sign-in code mail in the language of the person it goes to, from the
/// fixed templates: the subject has the site name only; the body has the
/// person's name, user name, the site and the link. The code is put in
/// last, once, after all of those, so it can only appear where the template
/// has `{code}`.
fn mail_text(
    lang: Lang,
    site: &str,
    name: &str,
    username: &str,
    url: &str,
    code: &str,
) -> (String, String) {
    let subject = crate::i18n::interpolate(
        crate::i18n::t_in(lang, "sign_in_code.mail_subject"),
        &[("site", &plain(site))],
    );
    let body = crate::i18n::interpolate(
        crate::i18n::t_in(lang, "sign_in_code.mail_body"),
        &[
            ("name", &plain(name)),
            ("username", &plain(username)),
            ("site", &plain(site)),
            ("url", &plain(url)),
        ],
    );
    (subject, body.replace("{code}", code))
}

pub(super) async fn create(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<ResetForm>,
) -> ApiResult<Response> {
    user.require_admin()?;
    if id == user.id {
        return Ok(back(jar, "info", crate::i18n::t("sign_in_code.err_self")));
    }
    let Some(target) = core.with_conn(move |c| db_users::get_user(c, id)).await? else {
        return Ok(back(jar, "error", crate::i18n::t("users.err_gone")));
    };
    if !target.active {
        return Ok(back(
            jar,
            "error",
            crate::i18n::t("sign_in_code.err_disabled"),
        ));
    }
    // The code is the way into the account: confirm it's you.
    if let Err(msg) =
        super::security::require_password(&core, &user, &jar, form.current_password).await?
    {
        return Ok(back(jar, "error", msg));
    }

    let code = sign_in_codes::generate();
    let (hash, admin) = (sign_in_codes::hash(&code), user.id);
    let send_email = form.send_email;
    core.with_conn(move |c| db_codes::insert(c, id, &hash, admin))
        .await?;
    tracing::info!(admin = %user.username, user = %target.username, "sign-in code created");

    let origin = match core.base_url().await {
        Some(url) => url.trim_end_matches('/').to_string(),
        None => super::invite::request_origin(&headers),
    };
    let url = format!("{origin}/sign-in-code");
    let (mut emailed_to, mut mail_failed) = (None, false);
    if send_email && let Some(address) = recipient(&core, id).await? {
        let lang = target
            .language
            .as_deref()
            .and_then(Lang::from_code)
            .unwrap_or_else(crate::i18n::site_default);
        let site = super::sign_in::site_name(&core).await;
        let (subject, body) = mail_text(lang, &site, target.name(), &target.username, &url, &code);
        match crate::mail::send(&core, &address, &subject, &body).await {
            Ok(()) => emailed_to = Some(address),
            Err(error) => {
                // What went wrong, never the message.
                tracing::warn!(user = %target.username, %error, "sign-in code email failed");
                mail_failed = true;
            }
        }
    }
    // A way into an account: the confirmation is used up.
    let jar = reauth::spend(jar);
    let new_code = NewCode {
        username: target.username,
        code,
        url,
        emailed_to,
        mail_failed,
    };
    super::users::render(&core, &user, jar, None, Some(new_code)).await
}

/// Ends the user's pending code ("Revoke code"): the admin's own password, or
/// a fresh grant, like creating one.
pub(super) async fn revoke(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    Form(form): Form<ResetForm>,
) -> ApiResult<Response> {
    user.require_admin()?;
    let Some(target) = core.with_conn(move |c| db_users::get_user(c, id)).await? else {
        return Ok(back(jar, "error", crate::i18n::t("users.err_gone")));
    };
    if let Err(msg) =
        super::security::require_password(&core, &user, &jar, form.current_password).await?
    {
        return Ok(back(jar, "error", msg));
    }
    let revoked = core
        .with_conn(move |c| db_codes::revoke_for_user(c, id))
        .await?;
    tracing::info!(admin = %user.username, user = %target.username, revoked, "sign-in code revoked");
    let jar = reauth::spend(jar);
    Ok(back(
        jar,
        "success",
        crate::i18n::tf("sign_in_code.revoked", &[("name", &target.username)]),
    ))
}

/// The address to email a code to: the user's, when the instance has mail
/// settings.
async fn recipient(core: &Core, user_id: i64) -> ApiResult<Option<String>> {
    if crate::mail::load(core).await?.is_none() {
        return Ok(None);
    }
    core.with_conn(move |c| db_codes::email(c, user_id)).await
}

// ---------------------------------------------------------------------------
// The user
// ---------------------------------------------------------------------------

/// `auth_layout.html` contract: `title` + `error`.
#[derive(Template)]
#[template(path = "sign_in_code.html")]
struct CodeTemplate {
    title: String,
    error: Option<String>,
    username: String,
    code: String,
    password_error: String,
    confirm_error: String,
}

impl CodeTemplate {
    fn blank() -> Self {
        Self {
            title: crate::i18n::t("sign_in_code.title").into(),
            error: None,
            username: String::new(),
            code: String::new(),
            password_error: String::new(),
            confirm_error: String::new(),
        }
    }
}

/// Keeps a page that takes a code out of caches and referrers.
fn private(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}

/// The one answer for a code that did not work, whatever the reason.
fn failed(username: &str) -> Response {
    let page = CodeTemplate {
        error: Some(crate::i18n::t("sign_in_code.invalid").into()),
        username: username.into(),
        ..CodeTemplate::blank()
    };
    private((StatusCode::UNAUTHORIZED, HtmlTemplate(page)).into_response())
}

pub(super) async fn form() -> Response {
    if let Some(ext) = super::external::paths() {
        // The product takes the password; it redeems the code.
        return Redirect::to(&format!("{}?code=1", ext.password_path)).into_response();
    }
    private(HtmlTemplate(CodeTemplate::blank()).into_response())
}

#[derive(Deserialize)]
pub struct RedeemForm {
    #[serde(default)]
    username: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    confirm: String,
}

/// The longest code text looked at: a code is 32 characters, with separators
/// a few more.
const MAX_CODE_INPUT: usize = 128;

pub(super) async fn submit(
    State(core): State<Core>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    PeerAddr(peer): PeerAddr,
    Form(form): Form<RedeemForm>,
) -> ApiResult<Response> {
    if super::external::paths().is_some() {
        return Err(ApiError::NotFound);
    }
    let username = form.username.trim().to_string();
    // The new password is checked first, and what is wrong with it is said:
    // that is about the password, not the code, and counts as no attempt.
    let mut page = CodeTemplate {
        username: username.clone(),
        code: form.code.chars().take(MAX_CODE_INPUT).collect(),
        ..CodeTemplate::blank()
    };
    if let Some(problem) = auth::password_problem(&form.password, &username) {
        page.password_error = problem.into();
    } else if form.password != form.confirm {
        page.confirm_error = crate::i18n::t("auth.passwords_mismatch").into();
    }
    if !page.password_error.is_empty() || !page.confirm_error.is_empty() {
        return Ok(private(
            (StatusCode::UNPROCESSABLE_ENTITY, HtmlTemplate(page)).into_response(),
        ));
    }

    let ip = session::client_ip(&headers, peer, session::trust_proxy());
    let code: String = form.code.chars().take(MAX_CODE_INPUT).collect();
    let redeemed = match sign_in_codes::attempt(&core, &username, &code, ip, Some(form.password))
        .await?
    {
        Ok(redeemed) => redeemed,
        Err(Refused::Throttled(wait)) => {
            tracing::warn!(user_ref = %sign_in_codes::log_ref(&username), ip = ?ip, "sign-in code attempts throttled");
            let page = CodeTemplate {
                error: Some(super::sign_in::wait_message(wait)),
                username,
                ..CodeTemplate::blank()
            };
            return Ok(private(
                (StatusCode::TOO_MANY_REQUESTS, HtmlTemplate(page)).into_response(),
            ));
        }
        Err(Refused::Generic) => {
            tracing::info!(user_ref = %sign_in_codes::log_ref(&username), ip = ?ip, "failed sign-in code");
            return Ok(failed(&username));
        }
    };

    let id = redeemed.user_id;
    let Some(user) = core
        .with_conn(move |c| {
            db_users::touch_last_login(c, id)?;
            db_users::get_user(c, id)
        })
        .await?
    else {
        return Ok(failed(&username));
    };
    tracing::info!(user = %user.username, "password reset with a sign-in code");
    let secure = secure_cookies(&headers);
    let jar = flash(jar, "success", crate::i18n::t("sign_in_code.done"));
    if user.totp_enabled {
        // The code replaces the password, nothing more: the second factor
        // is still asked.
        let pending = PendingLogin {
            user_id: user.id,
            version: redeemed.session_version,
            issued_at: session::now_unix(),
            next: "/".into(),
        };
        let jar = jar.add(session::pending_cookie(&pending, secure));
        return Ok((jar, Redirect::to("/login/2fa")).into_response());
    }
    let jar = login_jar(jar, user.id, redeemed.session_version, secure);
    Ok((jar, Redirect::to("/")).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mail_has_a_fixed_subject_and_the_code_only_in_the_body() {
        let code = "7GQ2-M4XB-0C5T-WZ9K-HE1N-Q8DY-A3RF-V6PS";
        // Names that try to pull the code in, to name another placeholder or
        // to break a line.
        let evil = "{code}{url}{{code}}\r\nBcc: evil@example.com";
        let (subject, body) = mail_text(
            Lang::EN,
            evil,
            evil,
            evil,
            "https://vole.example.org/sign-in-code",
            code,
        );
        assert!(
            !subject.contains(code) && !subject.contains("{code}"),
            "{subject}"
        );
        assert!(!subject.contains(['\r', '\n']), "{subject:?}");
        assert!(!subject.contains(['{', '}']), "{subject}");
        assert_eq!(body.matches(code).count(), 1, "{body}");
        assert!(
            !body.contains("{code}") && !body.contains("{url}"),
            "{body}"
        );
        assert!(!body.contains("{{"), "{body}");
        // A plain name comes through.
        let (subject, body) = mail_text(Lang::EN, "Ours", "Ada", "ada", "https://x.test/c", code);
        assert!(subject.contains("Ours"), "{subject}");
        assert!(
            body.contains("Hi Ada,") && body.contains("https://x.test/c"),
            "{body}"
        );
        assert!(!subject.contains(code));
    }
}
