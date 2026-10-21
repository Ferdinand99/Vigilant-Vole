//! Tag management (`/tags`). Owned by the monitors agent.
//!
//! Everyone can see tags; editors create, rename, recolour and delete them.
//! Create/edit happen in sheets (htmx): success reloads the page with a
//! flash toast, validation errors are swapped into the sheet.

use crate::i18n::Tr as _;
use askama::Template;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;

use super::{ApiError, ApiResult, AppState, HtmlTemplate, PageCtx, flash};
use crate::auth::{CurrentUser, Role};
use crate::db::tags::{self as db_tags, TagWithCount};

/// Protected routes (behind `require_auth`; `CurrentUser` is available).
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tags", get(list).post(create))
        .route("/tags/{id}", post(update).delete(delete))
}

#[derive(Template)]
#[template(path = "tags.html")]
struct TagsTemplate {
    ctx: PageCtx,
    tags: Vec<TagWithCount>,
    palette: Vec<(&'static str, String)>,
}

#[derive(Template)]
#[template(path = "tags_error.html")]
struct TagErrorTemplate {
    message: String,
}

#[derive(Deserialize, Default)]
struct TagForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    color: String,
}

fn is_htmx(headers: &HeaderMap) -> bool {
    headers.get("hx-request").and_then(|v| v.to_str().ok()) == Some("true")
}

/// Trimmed name, or a message saying how to fix it.
fn validate_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(crate::i18n::t("tags.err_name").into());
    }
    if name.chars().count() > db_tags::MAX_NAME_LEN {
        return Err(crate::i18n::tf(
            "validation.tag_name_long",
            &[("max", &db_tags::MAX_NAME_LEN.to_string())],
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(crate::i18n::t("tags.err_control").into());
    }
    Ok(name.to_string())
}

/// Success: reload the page (htmx) or redirect, with a flash toast.
fn done(jar: SignedCookieJar, headers: &HeaderMap, message: String) -> Response {
    let jar = flash(jar, "success", message);
    if is_htmx(headers) {
        let mut res = (jar, StatusCode::OK).into_response();
        res.headers_mut()
            .insert("hx-refresh", HeaderValue::from_static("true"));
        res
    } else {
        (jar, Redirect::to("/tags")).into_response()
    }
}

/// Validation failure: swap the message into the sheet (htmx) or flash it.
fn failed(
    jar: SignedCookieJar,
    headers: &HeaderMap,
    error_target: &str,
    message: String,
) -> Response {
    if is_htmx(headers) {
        let mut res = HtmlTemplate(TagErrorTemplate { message }).into_response();
        if let Ok(target) = HeaderValue::from_str(error_target) {
            res.headers_mut().insert("hx-retarget", target);
        }
        res.headers_mut()
            .insert("hx-reswap", HeaderValue::from_static("innerHTML"));
        res
    } else {
        (flash(jar, "error", message), Redirect::to("/tags")).into_response()
    }
}

async fn list(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<impl IntoResponse> {
    let tags = state
        .with_conn(|c| db_tags::list_tags_with_counts(c))
        .await?;
    let (ctx, jar) = PageCtx::build(
        &state,
        Some(&user),
        &jar,
        crate::i18n::t("dashboard.tags"),
        "dashboard",
        "",
    )
    .await;
    Ok((
        jar,
        HtmlTemplate(TagsTemplate {
            ctx,
            tags,
            palette: super::monitors::palette_options(),
        }),
    ))
}

async fn create(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Form(form): Form<TagForm>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let name = match validate_name(&form.name) {
        Ok(n) => n,
        Err(msg) => return Ok(failed(jar, &headers, "#new-tag-errors", msg)),
    };
    let color = db_tags::normalize_color(&form.color).to_string();
    let n = name.clone();
    let created = state
        .with_conn(move |c| {
            if db_tags::name_taken(c, &n, None)? {
                return Ok(false);
            }
            db_tags::insert_tag(c, &n, &color)?;
            Ok(true)
        })
        .await?;
    if !created {
        return Ok(failed(
            jar,
            &headers,
            "#new-tag-errors",
            crate::i18n::tf("tags.err_exists", &[("name", &name)]),
        ));
    }
    state.notify_update();
    Ok(done(
        jar,
        &headers,
        crate::i18n::tf("tags.created", &[("name", &name)]),
    ))
}

async fn update(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<TagForm>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let error_target = format!("#edit-tag-{id}-errors");
    let name = match validate_name(&form.name) {
        Ok(n) => n,
        Err(msg) => return Ok(failed(jar, &headers, &error_target, msg)),
    };
    let color = db_tags::normalize_color(&form.color).to_string();
    let n = name.clone();
    let outcome = state
        .with_conn(move |c| {
            if db_tags::get_tag(c, id)?.is_none() {
                return Ok(None);
            }
            if db_tags::name_taken(c, &n, Some(id))? {
                return Ok(Some(false));
            }
            db_tags::update_tag(c, id, &n, &color)?;
            Ok(Some(true))
        })
        .await?
        .ok_or(ApiError::NotFound)?;
    if !outcome {
        return Ok(failed(
            jar,
            &headers,
            &error_target,
            crate::i18n::tf("tags.err_exists", &[("name", &name)]),
        ));
    }
    state.notify_update();
    Ok(done(
        jar,
        &headers,
        crate::i18n::tf("tags.saved", &[("name", &name)]),
    ))
}

async fn delete(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let tag = state
        .with_conn(move |c| {
            let tag = db_tags::get_tag(c, id)?;
            if tag.is_some() {
                db_tags::delete_tag(c, id)?;
            }
            Ok(tag)
        })
        .await?
        .ok_or(ApiError::NotFound)?;
    state.notify_update();
    let message = crate::i18n::tf("tags.deleted", &[("name", &tag.name)]);
    if is_htmx(&headers) {
        let mut res = StatusCode::OK.into_response();
        if let Some(v) = super::monitors::toast_header(&message, "success") {
            res.headers_mut().insert("hx-trigger", v);
        }
        Ok(res)
    } else {
        Ok((flash(jar, "success", message), Redirect::to("/tags")).into_response())
    }
}

#[cfg(test)]
mod tests {
    use super::validate_name;

    #[test]
    fn tag_names_are_trimmed_and_bounded() {
        assert_eq!(validate_name("  prod ").unwrap(), "prod");
        assert!(validate_name("   ").is_err());
        assert!(validate_name(&"x".repeat(41)).is_err());
        assert!(validate_name("a\nb").is_err());
        assert_eq!(validate_name("Ünïcode 🚀").unwrap(), "Ünïcode 🚀");
    }
}
