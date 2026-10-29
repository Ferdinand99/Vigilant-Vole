//! User management (admin):
//! - `GET  /settings/users`, `POST /settings/users` (create)
//! - `POST /settings/users/{id}` (display name + role)
//! - `POST /settings/users/{id}/password` (set a temporary password)
//! - `POST /settings/users/{id}/2fa/reset`
//! - `POST /settings/users/{id}/sign-in-code` (one-time sign-in codes, see
//!   `sign_in_code`)
//!
//! Adding or promoting an administrator, resetting a password or
//! two-factor, and (with sign-in codes) creating or revoking a code or
//! changing someone's email address ask the admin for their own password
//! again.
//! - `POST /settings/users/{id}/passkeys/reset`
//! - `POST /settings/users/{id}/enable` · `/disable` · `/delete`
//!
//! The "last active admin" rule is enforced in `db::users`; admins also can't
//! change their own role, disable or delete themselves here.

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;

use crate::auth::{self, CurrentUser, Role, reauth};
use crate::db::api_tokens as db_tokens;
use crate::db::invites as db_invites;
use crate::db::passkeys as db_passkeys;
use crate::db::sign_in_codes as db_codes;
use crate::db::users::{self as db_users, User, UserChangeError};
use crate::http::{ApiResult, HtmlTemplate};
use crate::i18n::Tr as _;
use crate::page::{PageCtx, flash, initials};
use crate::state::Core;

use super::sign_in_code::NewCode;

/// One row of the user list.
struct UserRow {
    id: i64,
    username: String,
    display_name: String,
    name: String,
    initials: String,
    role: Role,
    totp_enabled: bool,
    /// How many passkeys the user has.
    passkeys: i64,
    active: bool,
    last_login_at: Option<String>,
    is_self: bool,
    /// The email address ("" for none; only read with
    /// [`KitFeatures::sign_in_codes`](crate::config::KitFeatures::sign_in_codes)).
    email: String,
    /// When the user's pending sign-in code expires (UTC), if they have one.
    code_until: Option<String>,
}

impl UserRow {
    fn from_user(
        u: User,
        current: i64,
        passkeys: &[(i64, i64)],
        emails: &[(i64, String)],
        codes: &[(i64, String)],
    ) -> Self {
        Self {
            code_until: codes
                .iter()
                .find(|(id, _)| *id == u.id)
                .map(|(_, until)| until.clone()),
            email: emails
                .iter()
                .find(|(id, _)| *id == u.id)
                .map(|(_, e)| e.clone())
                .unwrap_or_default(),
            passkeys: passkeys
                .iter()
                .find(|(id, _)| *id == u.id)
                .map_or(0, |(_, n)| *n),
            id: u.id,
            name: u.name().to_string(),
            initials: initials(u.name()),
            username: u.username,
            display_name: u.display_name,
            role: u.role,
            totp_enabled: u.totp_enabled,
            active: u.active,
            last_login_at: u.last_login_at,
            is_self: u.id == current,
        }
    }

    fn role_is(&self, role: &str) -> bool {
        self.role.as_str() == role
    }
}

#[derive(Template)]
#[template(path = "settings_users.html")]
struct UsersTemplate {
    ctx: PageCtx,
    users: Vec<UserRow>,
    roles: &'static [Role],
    /// Where the sheets ask for the admin's password.
    reauth: reauth::View,
    /// The kit sets no passwords (`CredentialMode::External`): no new user
    /// with a temporary password, no password reset.
    external: bool,
    /// Invitation links are on ([`KitFeatures::invites`]).
    ///
    /// [`KitFeatures::invites`]: crate::config::KitFeatures::invites
    invites: bool,
    /// The pending invitations.
    pending: Vec<db_invites::Invite>,
    /// The link of the invitation created by this request (shown once).
    new_invite: Option<String>,
    /// One-time sign-in codes are on
    /// ([`KitFeatures::sign_in_codes`](crate::config::KitFeatures::sign_in_codes)).
    sign_in_codes: bool,
    /// The instance can send mail (settings under General).
    smtp_ready: bool,
    /// The sign-in code created by this request (shown once).
    new_code: Option<NewCode>,
}

pub(super) async fn list(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<Response> {
    user.require_admin()?;
    render(&core, &user, jar, None, None).await
}

/// The Users page; `new_invite` is the link of an invitation just created,
/// `new_code` a sign-in code just created.
pub(super) async fn render(
    core: &Core,
    user: &CurrentUser,
    jar: SignedCookieJar,
    new_invite: Option<String>,
    new_code: Option<NewCode>,
) -> ApiResult<Response> {
    let invites = core.cfg.features.invites;
    let sign_in_codes = core.cfg.features.sign_in_codes;
    let (users, passkeys, pending, emails, codes) = core
        .with_conn(move |c| {
            Ok((
                db_users::list_users(c)?,
                db_passkeys::counts_by_user(c)?,
                if invites {
                    db_invites::list_pending(c)?
                } else {
                    Vec::new()
                },
                if sign_in_codes {
                    db_codes::emails(c)?
                } else {
                    Vec::new()
                },
                if sign_in_codes {
                    db_codes::pending_users(c)?
                } else {
                    Vec::new()
                },
            ))
        })
        .await?;
    let smtp_ready = sign_in_codes && crate::mail::load(core).await?.is_some();
    let rows = users
        .into_iter()
        .map(|u| UserRow::from_user(u, user.id, &passkeys, &emails, &codes))
        .collect();
    let (ctx, jar) = PageCtx::build(
        core,
        Some(user),
        &jar,
        crate::i18n::t("nav.users"),
        "settings",
        "users",
    )
    .await;
    let reauth = reauth::View::new(&jar, &core.session_key, user, "/settings/users");
    let secret = new_invite.is_some() || new_code.is_some();
    let mut response = (
        jar,
        HtmlTemplate(UsersTemplate {
            ctx,
            users: rows,
            roles: Role::all(),
            external: reauth.external,
            reauth,
            invites,
            pending,
            new_invite,
            sign_in_codes,
            smtp_ready,
            new_code,
        }),
    )
        .into_response();
    if secret {
        // The page holds a link or a code that works once: keep it out of
        // caches.
        response.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        );
    }
    Ok(response)
}

fn back(jar: SignedCookieJar, kind: &str, message: impl Into<String>) -> Response {
    (flash(jar, kind, message), Redirect::to("/settings/users")).into_response()
}

fn change_message(err: UserChangeError) -> &'static str {
    match err {
        UserChangeError::NotFound => crate::i18n::t("users.err_gone"),
        UserChangeError::LastAdmin => crate::i18n::t("users.err_last_admin"),
    }
}

fn parse_role(role: &str) -> Option<Role> {
    Role::parse(role.trim())
}

#[derive(Deserialize)]
pub struct CreateForm {
    #[serde(default)]
    username: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    password: String,
    /// The admin's own password, needed to add an administrator.
    #[serde(default)]
    current_password: String,
}

pub(super) async fn create(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Form(form): Form<CreateForm>,
) -> ApiResult<Response> {
    user.require_admin()?;
    let username = form.username.trim().to_string();
    let display_name = form.display_name.trim().to_string();
    if let Some(problem) = auth::username_problem(&username) {
        return Ok(back(
            jar,
            "error",
            crate::i18n::tf("users.err_username", &[("problem", problem)]),
        ));
    }
    if display_name.chars().count() > 80 {
        return Ok(back(jar, "error", crate::i18n::t("users.err_display_long")));
    }
    let Some(role) = parse_role(&form.role) else {
        return Ok(back(jar, "error", crate::i18n::t("users.err_role")));
    };
    if let Some(problem) = auth::password_problem(&form.password, &username) {
        return Ok(back(
            jar,
            "error",
            crate::i18n::tf("users.err_temp_password", &[("problem", problem)]),
        ));
    }
    let lookup = username.clone();
    if core
        .with_conn(move |c| db_users::username_taken(c, &lookup))
        .await?
    {
        return Ok(back(
            jar,
            "error",
            crate::i18n::tf("users.err_taken", &[("name", &username)]),
        ));
    }
    // A new administrator outlasts a stolen session: confirm it's you.
    if role == Role::Admin
        && let Err(msg) =
            super::security::require_password(&core, &user, &jar, form.current_password).await?
    {
        return Ok(back(jar, "error", msg));
    }
    let hash = auth::hash_password_async(form.password).await?;
    let (name, shown) = (username.clone(), display_name.clone());
    core.with_conn(move |c| db_users::insert_user_full(c, &name, &hash, role, &shown))
        .await?;
    tracing::info!(admin = %user.username, new_user = %username, role = %role, "user created");
    Ok(back(
        jar,
        "success",
        crate::i18n::tf("users.added", &[("name", &username)]),
    ))
}

#[derive(Deserialize)]
pub struct UpdateForm {
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    role: String,
    /// The admin's own password, needed to make someone an administrator
    /// and, with sign-in codes, to change an email address.
    #[serde(default)]
    current_password: String,
    /// The user's email address with
    /// [`KitFeatures::sign_in_codes`](crate::config::KitFeatures::sign_in_codes);
    /// empty clears it, a form without the field leaves it alone.
    #[serde(default)]
    email: Option<String>,
}

pub(super) async fn update(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    Form(form): Form<UpdateForm>,
) -> ApiResult<Response> {
    user.require_admin()?;
    let display_name = form.display_name.trim().to_string();
    if display_name.chars().count() > 80 {
        return Ok(back(jar, "error", crate::i18n::t("users.err_display_long")));
    }
    let Some(role) = parse_role(&form.role) else {
        return Ok(back(jar, "error", crate::i18n::t("users.err_role")));
    };
    if id == user.id && role != user.role {
        return Ok(back(jar, "error", crate::i18n::t("users.err_own_role")));
    }
    // An address, when the feature is on and the form has the field.
    let email = form
        .email
        .as_deref()
        .map(str::trim)
        .filter(|_| core.cfg.features.sign_in_codes)
        .map(str::to_string);
    if let Some(email) = &email
        && !email.is_empty()
        && !crate::mail::is_address(email)
    {
        return Ok(back(jar, "error", crate::i18n::t("sign_in_code.err_email")));
    }
    // Promoting is as good as adding an administrator.
    let promoting = role == Role::Admin
        && core
            .with_conn(move |c| db_users::get_user(c, id))
            .await?
            .is_some_and(|target| target.role != Role::Admin);
    // A code goes to the address stored here: changing it is as good as
    // taking the account, so it asks the admin's password too.
    let new_email = email.clone();
    let email_changes = match new_email {
        Some(new) => {
            core.with_conn(move |c| Ok(db_codes::email(c, id)?.unwrap_or_default() != new))
                .await?
        }
        None => false,
    };
    if (promoting || email_changes)
        && let Err(msg) =
            super::security::require_password(&core, &user, &jar, form.current_password).await?
    {
        return Ok(back(jar, "error", msg));
    }
    let result = core
        .with_conn(move |c| {
            let result = db_users::update_profile(c, id, &display_name, role)?;
            if result.is_ok()
                && let Some(email) = &email
            {
                db_codes::set_email(c, id, Some(email))?;
            }
            Ok(result)
        })
        .await?;
    match result {
        Ok(()) => {
            tracing::info!(admin = %user.username, user_id = id, role = %role, "user updated");
            // An administrator made outlasts a stolen session, and so does
            // an address that receives codes: the confirmation is used up.
            let jar = if promoting || email_changes {
                reauth::spend(jar)
            } else {
                jar
            };
            Ok(back(jar, "success", crate::i18n::t("users.saved")))
        }
        Err(err) => Ok(back(jar, "error", change_message(err))),
    }
}

#[derive(Deserialize)]
pub struct ResetPasswordForm {
    #[serde(default)]
    password: String,
    /// The admin's own password.
    #[serde(default)]
    current_password: String,
    /// Also revoke the user's API tokens (the sheet's box is on by default;
    /// a form that doesn't send it keeps them).
    #[serde(default)]
    revoke_tokens: bool,
}

/// The admin's own password, for the actions that take over or unlock
/// someone's sign-in.
#[derive(Deserialize)]
pub struct ConfirmForm {
    #[serde(default)]
    current_password: String,
}

pub(super) async fn reset_password(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    Form(form): Form<ResetPasswordForm>,
) -> ApiResult<Response> {
    user.require_admin()?;
    if id == user.id {
        return Ok(back(jar, "info", crate::i18n::t("users.own_password")));
    }
    let Some(target) = core.with_conn(move |c| db_users::get_user(c, id)).await? else {
        return Ok(back(
            jar,
            "error",
            change_message(UserChangeError::NotFound),
        ));
    };
    if let Some(problem) = auth::password_problem(&form.password, &target.username) {
        return Ok(back(
            jar,
            "error",
            crate::i18n::tf("users.err_temp_password", &[("problem", problem)]),
        ));
    }
    if let Err(msg) =
        super::security::require_password(&core, &user, &jar, form.current_password).await?
    {
        return Ok(back(jar, "error", msg));
    }
    let hash = auth::hash_password_async(form.password).await?;
    let revoke = form.revoke_tokens;
    let revoked = core
        .with_conn(move |c| {
            db_users::set_password(c, id, &hash)?;
            if revoke {
                db_tokens::delete_all_for_user(c, id)
            } else {
                Ok(0)
            }
        })
        .await?;
    tracing::info!(admin = %user.username, user = %target.username, revoked_tokens = revoked, "password reset by admin");
    Ok(back(
        jar,
        "success",
        crate::i18n::tf("users.password_reset", &[("name", &target.username)]),
    ))
}

pub(super) async fn reset_two_factor(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    Form(form): Form<ConfirmForm>,
) -> ApiResult<Response> {
    user.require_admin()?;
    let Some(target) = core.with_conn(move |c| db_users::get_user(c, id)).await? else {
        return Ok(back(
            jar,
            "error",
            change_message(UserChangeError::NotFound),
        ));
    };
    if let Err(msg) =
        super::security::require_password(&core, &user, &jar, form.current_password).await?
    {
        return Ok(back(jar, "error", msg));
    }
    core.with_conn(move |c| {
        db_users::disable_totp(c, id)?;
        db_users::bump_session_version(c, id)
    })
    .await?;
    tracing::info!(admin = %user.username, user = %target.username, "two-factor reset by admin");
    if id == user.id {
        // Our own session version moved on; log in again.
        return Ok(Redirect::to("/login").into_response());
    }
    Ok(back(
        jar,
        "success",
        crate::i18n::tf("users.tfa_reset", &[("name", &target.username)]),
    ))
}

/// Removes all of a user's passkeys (they lost a device, or it was
/// stolen) and signs them out everywhere, like the two-factor reset.
pub(super) async fn reset_passkeys(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require_admin()?;
    let Some(target) = core.with_conn(move |c| db_users::get_user(c, id)).await? else {
        return Ok(back(
            jar,
            "error",
            change_message(UserChangeError::NotFound),
        ));
    };
    let removed = core
        .with_conn(move |c| {
            let removed = db_passkeys::delete_all_for_user(c, id)?;
            db_users::bump_session_version(c, id)?;
            Ok(removed)
        })
        .await?;
    tracing::info!(admin = %user.username, user = %target.username, removed, "passkeys removed by admin");
    if id == user.id {
        // Our own session version moved on; log in again.
        return Ok(Redirect::to("/login").into_response());
    }
    Ok(back(
        jar,
        "success",
        crate::i18n::tf("users.pk_reset", &[("name", &target.username)]),
    ))
}

async fn set_active(
    core: Core,
    user: CurrentUser,
    jar: SignedCookieJar,
    id: i64,
    active: bool,
) -> ApiResult<Response> {
    user.require_admin()?;
    if id == user.id && !active {
        return Ok(back(jar, "error", crate::i18n::t("users.err_disable_self")));
    }
    let result = core
        .with_conn(move |c| db_users::set_active(c, id, active))
        .await?;
    match result {
        Ok(()) => {
            tracing::info!(admin = %user.username, user_id = id, active, "user active state changed");
            Ok(back(
                jar,
                "success",
                if active {
                    crate::i18n::t("users.enabled")
                } else {
                    crate::i18n::t("users.disabled_flash")
                },
            ))
        }
        Err(err) => Ok(back(jar, "error", change_message(err))),
    }
}

pub(super) async fn enable(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    set_active(core, user, jar, id, true).await
}

pub(super) async fn disable(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    set_active(core, user, jar, id, false).await
}

pub(super) async fn delete(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require_admin()?;
    if id == user.id {
        return Ok(back(jar, "error", crate::i18n::t("users.err_delete_self")));
    }
    let result = core
        .with_conn(move |c| db_users::delete_user(c, id))
        .await?;
    match result {
        Ok(()) => {
            tracing::info!(admin = %user.username, user_id = id, "user deleted");
            Ok(back(jar, "success", crate::i18n::t("users.deleted")))
        }
        Err(err) => Ok(back(jar, "error", change_message(err))),
    }
}
