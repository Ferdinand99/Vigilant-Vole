//! API tokens. Owned by the auth agent.
//!
//! Routes:
//! - `GET  /settings/tokens` — your tokens (admins also see everyone's)
//! - `POST /settings/tokens` — create; the response shows the token once
//! - `DELETE /settings/tokens/{id}` — revoke (owner or admin; htmx row removal)
//!
//! Tokens authenticate `/api/v1/*` and `/metrics` (see `api::auth`).

use crate::i18n::Tr as _;
use askama::Template;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get};
use axum::{Form, Router};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;

use super::{ApiError, ApiResult, AppState, HtmlTemplate, PageCtx, flash};
use crate::auth::{CurrentUser, tokens};
use crate::db::api_tokens::{self as db_tokens, ApiToken, TokenScope};

/// Per-user cap, so a script gone wrong can't fill the table.
const MAX_TOKENS_PER_USER: usize = 50;
/// Allowed expiry choices in days (`None` = never).
const EXPIRY_CHOICES: &[(&str, Option<i64>)] = &[("30", Some(30)), ("90", Some(90)), ("365", Some(365)), ("never", None)];

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/settings/tokens", get(list).post(create))
        .route("/settings/tokens/{id}", delete(revoke))
}

struct TokenRow {
    id: i64,
    name: String,
    prefix: String,
    username: String,
    scope: &'static str,
    scope_label: &'static str,
    created_at: String,
    last_used_at: Option<String>,
    expires_at: Option<String>,
    expired: bool,
}

impl From<ApiToken> for TokenRow {
    fn from(t: ApiToken) -> Self {
        Self {
            id: t.id,
            name: t.name,
            prefix: t.prefix,
            username: t.username,
            scope: t.scope.as_str(),
            scope_label: t.scope.label(),
            created_at: t.created_at,
            last_used_at: t.last_used_at,
            expires_at: t.expires_at,
            expired: t.expired,
        }
    }
}

#[derive(Template)]
#[template(path = "settings_tokens.html")]
struct TokensTemplate {
    ctx: PageCtx,
    tokens: Vec<TokenRow>,
    /// Other users' tokens (admins only).
    others: Vec<TokenRow>,
    /// The plaintext of a token created in this request (shown once).
    new_token: Option<String>,
    new_token_name: String,
    base_url: String,
}

async fn render(state: &AppState, user: &CurrentUser, jar: SignedCookieJar, created: Option<(String, String)>) -> ApiResult<Response> {
    let uid = user.id;
    let is_admin = user.is_admin();
    let (mine, all) = state
        .with_conn(move |c| {
            let mine = db_tokens::list_for_user(c, uid)?;
            let all = if is_admin { db_tokens::list_all(c)? } else { Vec::new() };
            Ok((mine, all))
        })
        .await?;
    let others = all.into_iter().filter(|t| t.user_id != uid).map(TokenRow::from).collect();
    let base_url = state.settings().await?.base_url.unwrap_or_default();
    let (ctx, jar) = PageCtx::build(state, Some(user), &jar, crate::i18n::t("nav.api_tokens"), "settings", "tokens").await;
    let (new_token, new_token_name) = match created {
        Some((token, name)) => (Some(token), name),
        None => (None, String::new()),
    };
    let page = TokensTemplate {
        ctx,
        tokens: mine.into_iter().map(TokenRow::from).collect(),
        others,
        new_token,
        new_token_name,
        base_url,
    };
    // The page may contain a secret: keep it out of caches and history snapshots.
    let mut response = (jar, HtmlTemplate(page)).into_response();
    response
        .headers_mut()
        .insert(axum::http::header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn list(State(state): State<AppState>, user: CurrentUser, jar: SignedCookieJar) -> ApiResult<Response> {
    user.require(crate::auth::Role::Viewer)?;
    render(&state, &user, jar, None).await
}

#[derive(Deserialize)]
pub struct CreateForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    expires: String,
}

fn back(jar: SignedCookieJar, kind: &str, message: &str) -> Response {
    (flash(jar, kind, message), Redirect::to("/settings/tokens")).into_response()
}

async fn create(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Form(form): Form<CreateForm>,
) -> ApiResult<Response> {
    user.require(crate::auth::Role::Viewer)?;
    let name = form.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 80 {
        return Ok(back(jar, "error", crate::i18n::t("tokens.err_name")));
    }
    let Some(scope) = TokenScope::parse(form.scope.trim()) else {
        return Ok(back(jar, "error", crate::i18n::t("tokens.err_scope")));
    };
    let Some(expires_in) = EXPIRY_CHOICES
        .iter()
        .find(|(key, _)| *key == form.expires.trim())
        .map(|(_, days)| *days)
    else {
        return Ok(back(jar, "error", crate::i18n::t("tokens.err_expiry")));
    };
    let uid = user.id;
    let count = state.with_conn(move |c| db_tokens::list_for_user(c, uid)).await?.len();
    if count >= MAX_TOKENS_PER_USER {
        return Ok(back(jar, "error", crate::i18n::t("tokens.err_limit")));
    }

    let token = tokens::generate();
    let (hash, prefix, stored_name) = (tokens::hash(&token), tokens::display_prefix(&token), name.clone());
    state
        .with_conn(move |c| db_tokens::insert(c, uid, &stored_name, &prefix, &hash, scope, expires_in))
        .await?;
    tracing::info!(user = %user.username, token = %name, scope = scope.as_str(), "API token created");
    let jar = flash(jar, "success", crate::i18n::t("tokens.created"));
    render(&state, &user, jar, Some((token, name))).await
}

async fn revoke(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    let token = state
        .with_conn(move |c| db_tokens::get(c, id))
        .await?
        .ok_or(ApiError::NotFound)?;
    // Your own tokens, or anyone's if you're an admin. Don't reveal others' ids.
    if token.user_id != user.id && !user.is_admin() {
        return Err(ApiError::NotFound);
    }
    state.with_conn(move |c| db_tokens::delete(c, id)).await?;
    tracing::info!(user = %user.username, token = %token.name, owner = %token.username, "API token revoked");

    if headers.contains_key("hx-request") {
        let mut response = StatusCode::OK.into_response();
        let value = super::status_pages::toast_trigger(crate::i18n::t("tokens.revoked"), "success", false);
        response.headers_mut().insert("hx-trigger", value);
        return Ok(response);
    }
    Ok(Redirect::to("/settings/tokens").into_response())
}

#[cfg(test)]
mod tests {
    use askama::Template as _;

    use super::{PageCtx, TokensTemplate};
    use crate::api::page::UserView;
    use crate::auth::Role;

    /// Regression: the shown-once token's copy field reused the sheet's id
    /// (`new-token`), so right after creating a token the "New token" button
    /// targeted the `<code>` element and the sheet no longer opened.
    #[test]
    fn created_token_page_keeps_element_ids_unique() {
        let page = TokensTemplate {
            ctx: PageCtx {
                title: "API tokens".into(),
                nav: "settings",
                subnav: "tokens",
                user: Some(UserView {
                    id: 1,
                    username: "admin".into(),
                    display_name: "admin".into(),
                    role: Role::Admin,
                    initials: "A".into(),
                }),
                site_name: "Vigilant Vole".into(),
                flash: None,
                lang: "en".into(),
            },
            tokens: Vec::new(),
            others: Vec::new(),
            new_token: Some("vv_secret".into()),
            new_token_name: "CI".into(),
            base_url: String::new(),
        };
        let html = page.render().expect("template renders");
        assert_eq!(html.matches(r#"id="new-token""#).count(), 1, "duplicate id in tokens page");
        assert!(html.contains(r#"<dialog class="sheet" id="new-token""#));
        assert!(html.contains("vv_secret"));
    }
}
