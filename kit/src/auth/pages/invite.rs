//! Invitation links ([`auth::invites`](crate::auth::invites)):
//! - `POST /settings/invites` (admin): creates one and shows its link once
//! - `POST /settings/invites/{id}/revoke` (admin)
//! - `GET /invite/{token}`, `POST /invite/{token}` (public): the invitee picks
//!   a user name and a password and is signed in. With
//!   `CredentialMode::External` the GET leads to the product's setup page
//!   instead and the POST is gone.
//!
//! A token that is expired, used, revoked or unknown always gets the same
//! page ([`not_valid`]).

use std::time::Instant;

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, REFERRER_POLICY};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;

use crate::auth::guard::{PeerAddr, login_jar, secure_cookies};
use crate::auth::invites::{self, AcceptError};
use crate::auth::{self, CurrentUser, Role, session};
use crate::db::invites as db_invites;
use crate::db::users as db_users;
use crate::http::{ApiError, ApiResult, HtmlTemplate};
use crate::i18n::Tr as _;
use crate::page::flash;
use crate::state::Core;

// ---------------------------------------------------------------------------
// Admin
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateForm {
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    role: String,
    /// The admin's own password (`CredentialMode::Plain`).
    #[serde(default)]
    current_password: String,
}

fn back(jar: SignedCookieJar, kind: &str, message: impl Into<String>) -> Response {
    (flash(jar, kind, message), Redirect::to("/settings/users")).into_response()
}

/// `https://host` of this request, for a link when the site has no public
/// URL set.
fn request_origin(headers: &HeaderMap) -> String {
    let scheme = if secure_cookies(headers) {
        "https"
    } else {
        "http"
    };
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .filter(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_graphic()))
        .unwrap_or("localhost");
    format!("{scheme}://{host}")
}

pub(super) async fn create(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Form(form): Form<CreateForm>,
) -> ApiResult<Response> {
    user.require_admin()?;
    let display_name = form.display_name.trim().to_string();
    if display_name.chars().count() > 80 {
        return Ok(back(jar, "error", crate::i18n::t("users.err_display_long")));
    }
    let Some(role) = Role::parse(form.role.trim()) else {
        return Ok(back(jar, "error", crate::i18n::t("users.err_role")));
    };
    // A link is an account for whoever opens it: confirm it's you, whatever
    // the role.
    if let Err(msg) =
        super::security::require_password(&core, &user, &jar, form.current_password).await?
    {
        return Ok(back(jar, "error", msg));
    }
    let token = invites::generate();
    let (hash, creator, shown) = (invites::hash(&token), user.id, display_name);
    let stored = core
        .with_conn(move |c| {
            let tx = c.transaction()?;
            if db_invites::count_pending(&tx)? >= db_invites::MAX_PENDING {
                return Ok(false);
            }
            db_invites::insert(&tx, &hash, role, &shown, creator)?;
            tx.commit()?;
            Ok(true)
        })
        .await?;
    if !stored {
        return Ok(back(jar, "error", crate::i18n::t("invite.err_limit")));
    }
    tracing::info!(admin = %user.username, role = %role, "invitation created");
    let origin = match core.base_url().await {
        Some(url) => url.trim_end_matches('/').to_string(),
        None => request_origin(&headers),
    };
    let link = format!("{origin}/invite/{token}");
    let jar = flash(jar, "success", crate::i18n::t("invite.created"));
    super::users::render(&core, &user, jar, Some(link)).await
}

pub(super) async fn revoke(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require_admin()?;
    let revoked = core.with_conn(move |c| db_invites::revoke(c, id)).await?;
    if !revoked {
        return Ok(back(jar, "error", crate::i18n::t("invite.err_gone")));
    }
    tracing::info!(admin = %user.username, invite = id, "invitation revoked");
    Ok(back(jar, "success", crate::i18n::t("invite.revoked")))
}

// ---------------------------------------------------------------------------
// The invitee
// ---------------------------------------------------------------------------

/// `auth_layout.html` contract: `title` + `error`.
#[derive(Template)]
#[template(path = "invite.html")]
struct InviteTemplate {
    title: String,
    error: Option<String>,
    /// The form is shown; otherwise the invitation is not valid.
    valid: bool,
    token: String,
    /// "You're invited to join as {role}."
    joining: String,
    username: String,
    display_name: String,
    username_error: String,
    password_error: String,
    confirm_error: String,
}

impl InviteTemplate {
    fn form(token: &str, role: Role, display_name: &str) -> Self {
        Self {
            title: crate::i18n::t("invite.title").into(),
            error: None,
            valid: true,
            token: token.into(),
            joining: crate::i18n::tf("invite.joining", &[("role", role.label())]),
            username: String::new(),
            display_name: display_name.into(),
            username_error: String::new(),
            password_error: String::new(),
            confirm_error: String::new(),
        }
    }

    fn has_errors(&self) -> bool {
        [
            &self.username_error,
            &self.password_error,
            &self.confirm_error,
        ]
        .iter()
        .any(|e| !e.is_empty())
    }
}

/// Keeps a page with a token in its address out of caches and referrers.
fn private(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}

/// The one answer for an expired, used, revoked or unknown invitation.
fn not_valid(status: StatusCode, wait: Option<String>) -> Response {
    let page = InviteTemplate {
        title: crate::i18n::t("invite.invalid_title").into(),
        error: Some(wait.unwrap_or_else(|| crate::i18n::t("invite.invalid").into())),
        valid: false,
        token: String::new(),
        joining: String::new(),
        username: String::new(),
        display_name: String::new(),
        username_error: String::new(),
        password_error: String::new(),
        confirm_error: String::new(),
    };
    private((status, HtmlTemplate(page)).into_response())
}

pub(super) async fn form(
    State(core): State<Core>,
    Path(token): Path<String>,
) -> ApiResult<Response> {
    let Some(invite) = invites::lookup(&core, &token).await? else {
        return Ok(not_valid(StatusCode::NOT_FOUND, None));
    };
    if let Some(ext) = super::external::paths() {
        // The product registers the user; it checks the token again.
        return Ok(private(
            Redirect::to(&format!("{}?invite={token}", ext.setup_path)).into_response(),
        ));
    }
    let page = InviteTemplate::form(&token, invite.role, &invite.display_name);
    Ok(private(HtmlTemplate(page).into_response()))
}

#[derive(Deserialize)]
pub struct AcceptForm {
    #[serde(default)]
    username: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    confirm: String,
}

pub(super) async fn submit(
    State(core): State<Core>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    PeerAddr(peer): PeerAddr,
    Path(token): Path<String>,
    Form(form): Form<AcceptForm>,
) -> ApiResult<Response> {
    if super::external::paths().is_some() {
        return Err(ApiError::NotFound);
    }
    // Per client address, like the other pre-sign-in forms. The attempt is
    // counted first; a valid token with a form to correct is not a guess and
    // is handed back, a finished sign-up clears the count.
    let key =
        session::client_ip(&headers, peer, session::trust_proxy()).map(|ip| format!("invite:{ip}"));
    if let Some(key) = &key
        && let Err(wait) = core.limits.ip.reserve(key, Instant::now())
    {
        tracing::warn!(ip = ?key, "invitation attempts throttled");
        return Ok(not_valid(
            StatusCode::TOO_MANY_REQUESTS,
            Some(super::sign_in::wait_message(wait)),
        ));
    }
    let release = || {
        if let Some(key) = &key {
            core.limits.ip.release(key);
        }
    };
    let Some(invite) = invites::lookup(&core, &token).await? else {
        return Ok(not_valid(StatusCode::NOT_FOUND, None));
    };

    let username = form.username.trim().to_string();
    let display_name = form.display_name.trim().to_string();
    let mut page = InviteTemplate::form(&token, invite.role, &display_name);
    page.username = username.clone();
    if let Some(problem) = auth::username_problem(&username) {
        page.username_error = problem.into();
    }
    if display_name.chars().count() > 80 {
        page.error = Some(crate::i18n::t("users.err_display_long").into());
    }
    if let Some(problem) = auth::password_problem(&form.password, &username) {
        page.password_error = problem.into();
    } else if form.password != form.confirm {
        page.confirm_error = crate::i18n::t("auth.passwords_mismatch").into();
    }
    if page.has_errors() || page.error.is_some() {
        release();
        return Ok(private(
            (StatusCode::UNPROCESSABLE_ENTITY, HtmlTemplate(page)).into_response(),
        ));
    }

    let password_hash = auth::hash_password_async(form.password).await?;
    match invites::accept(&core, &token, &username, &display_name, &password_hash).await? {
        Ok(user_id) => {
            if let Some(key) = &key {
                core.limits.ip.clear(key);
            }
            let user = core
                .with_conn(move |c| {
                    db_users::touch_last_login(c, user_id)?;
                    db_users::get_user(c, user_id)
                })
                .await?
                .ok_or(ApiError::NotFound)?;
            tracing::info!(user = %user.username, role = %user.role, "account created from an invitation");
            let jar = login_jar(jar, user.id, user.session_version, secure_cookies(&headers));
            let jar = flash(jar, "success", crate::i18n::t("invite.joined"));
            Ok((jar, Redirect::to("/")).into_response())
        }
        // Used by someone else in the meantime.
        Err(AcceptError::Invalid) => Ok(not_valid(StatusCode::NOT_FOUND, None)),
        Err(AcceptError::UsernameTaken) => {
            release();
            page.username_error = crate::i18n::tf("users.err_taken", &[("name", &username)]);
            Ok(private(
                (StatusCode::UNPROCESSABLE_ENTITY, HtmlTemplate(page)).into_response(),
            ))
        }
    }
}
