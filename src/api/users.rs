//! User management (admin). Owned by the auth agent.
//!
//! Routes (all admin-only, plain form posts → flash + redirect):
//! - `GET  /settings/users`, `POST /settings/users` (create)
//! - `POST /settings/users/{id}` (display name + role)
//! - `POST /settings/users/{id}/password` (set a temporary password)
//! - `POST /settings/users/{id}/2fa/reset`
//! - `POST /settings/users/{id}/enable` · `/disable` · `/delete`
//!
//! The "last active admin" rule is enforced in `db::users`; admins also can't
//! change their own role, disable or delete themselves here.

use crate::i18n::Tr as _;
use askama::Template;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;

use super::page::initials;
use super::{ApiResult, AppState, HtmlTemplate, PageCtx, flash};
use crate::auth::{self, CurrentUser, Role};
use crate::db::users::{self as db_users, User, UserChangeError};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/settings/users", get(list).post(create))
        .route("/settings/users/{id}", post(update))
        .route("/settings/users/{id}/password", post(reset_password))
        .route("/settings/users/{id}/2fa/reset", post(reset_two_factor))
        .route("/settings/users/{id}/enable", post(enable))
        .route("/settings/users/{id}/disable", post(disable))
        .route("/settings/users/{id}/delete", post(delete))
}

/// One row of the user list.
struct UserRow {
    id: i64,
    username: String,
    display_name: String,
    name: String,
    initials: String,
    role: Role,
    totp_enabled: bool,
    active: bool,
    last_login_at: Option<String>,
    is_self: bool,
}

impl UserRow {
    fn from_user(u: User, current: i64) -> Self {
        Self {
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
}

async fn list(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<Response> {
    user.require_admin()?;
    let users = state.with_conn(|c| db_users::list_users(c)).await?;
    let rows = users
        .into_iter()
        .map(|u| UserRow::from_user(u, user.id))
        .collect();
    let (ctx, jar) = PageCtx::build(
        &state,
        Some(&user),
        &jar,
        crate::i18n::t("nav.users"),
        "settings",
        "users",
    )
    .await;
    Ok((
        jar,
        HtmlTemplate(UsersTemplate {
            ctx,
            users: rows,
            roles: Role::all(),
        }),
    )
        .into_response())
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
}

async fn create(
    State(state): State<AppState>,
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
    if state
        .with_conn(move |c| db_users::username_taken(c, &lookup))
        .await?
    {
        return Ok(back(
            jar,
            "error",
            crate::i18n::tf("users.err_taken", &[("name", &username)]),
        ));
    }
    let hash = auth::hash_password_async(form.password).await?;
    let (name, shown) = (username.clone(), display_name.clone());
    state
        .with_conn(move |c| db_users::insert_user_full(c, &name, &hash, role, &shown))
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
}

async fn update(
    State(state): State<AppState>,
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
    let result = state
        .with_conn(move |c| db_users::update_profile(c, id, &display_name, role))
        .await?;
    match result {
        Ok(()) => {
            tracing::info!(admin = %user.username, user_id = id, role = %role, "user updated");
            Ok(back(jar, "success", crate::i18n::t("users.saved")))
        }
        Err(err) => Ok(back(jar, "error", change_message(err))),
    }
}

#[derive(Deserialize)]
pub struct ResetPasswordForm {
    #[serde(default)]
    password: String,
}

async fn reset_password(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    Form(form): Form<ResetPasswordForm>,
) -> ApiResult<Response> {
    user.require_admin()?;
    if id == user.id {
        return Ok(back(jar, "info", crate::i18n::t("users.own_password")));
    }
    let Some(target) = state.with_conn(move |c| db_users::get_user(c, id)).await? else {
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
    let hash = auth::hash_password_async(form.password).await?;
    state
        .with_conn(move |c| db_users::set_password(c, id, &hash))
        .await?;
    tracing::info!(admin = %user.username, user = %target.username, "password reset by admin");
    Ok(back(
        jar,
        "success",
        crate::i18n::tf("users.password_reset", &[("name", &target.username)]),
    ))
}

async fn reset_two_factor(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require_admin()?;
    let Some(target) = state.with_conn(move |c| db_users::get_user(c, id)).await? else {
        return Ok(back(
            jar,
            "error",
            change_message(UserChangeError::NotFound),
        ));
    };
    state
        .with_conn(move |c| {
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

async fn set_active(
    state: AppState,
    user: CurrentUser,
    jar: SignedCookieJar,
    id: i64,
    active: bool,
) -> ApiResult<Response> {
    user.require_admin()?;
    if id == user.id && !active {
        return Ok(back(jar, "error", crate::i18n::t("users.err_disable_self")));
    }
    let result = state
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

async fn enable(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    set_active(state, user, jar, id, true).await
}

async fn disable(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    set_active(state, user, jar, id, false).await
}

async fn delete(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require_admin()?;
    if id == user.id {
        return Ok(back(jar, "error", crate::i18n::t("users.err_delete_self")));
    }
    let result = state
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
