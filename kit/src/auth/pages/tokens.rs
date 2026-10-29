//! API tokens:
//! - `GET  /settings/tokens`: your tokens (admins also see everyone's)
//! - `POST /settings/tokens`: create; the response shows the token once
//! - `DELETE /settings/tokens/{id}`: revoke (owner or admin; htmx row removal)

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;

use crate::auth::{CurrentUser, reauth, tokens};
use crate::db::api_tokens::{self as db_tokens, ApiToken, TokenScope};
use crate::http::{ApiError, ApiResult, HtmlTemplate};
use crate::i18n::Tr as _;
use crate::page::{PageCtx, flash};
use crate::state::Core;

/// Per-user cap, so a script gone wrong can't fill the table.
const MAX_TOKENS_PER_USER: usize = 50;
/// Allowed expiry choices in days (`None` = never).
const EXPIRY_CHOICES: &[(&str, Option<i64>)] = &[
    ("30", Some(30)),
    ("90", Some(90)),
    ("365", Some(365)),
    ("never", None),
];

struct TokenRow {
    id: i64,
    name: String,
    prefix: String,
    username: String,
    scope: &'static str,
    scope_label: String,
    created_at: String,
    last_used_at: Option<String>,
    expires_at: Option<String>,
    expired: bool,
}

/// The Access badge: the product scopes' labels when the token has any,
/// else "Read only" or "Read and write".
fn access_label(t: &ApiToken) -> String {
    if t.scopes.is_empty() {
        return t.scope.label().to_string();
    }
    t.scopes
        .iter()
        .map(|name| match crate::config::get().token_scope(name) {
            Some(scope) => crate::i18n::t(scope.label),
            None => name.as_str(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

impl From<ApiToken> for TokenRow {
    fn from(t: ApiToken) -> Self {
        Self {
            scope_label: access_label(&t),
            id: t.id,
            name: t.name,
            prefix: t.prefix,
            username: t.username,
            scope: t.scope.as_str(),
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
    /// What the `curl` example calls, and where Prometheus scrapes (if anywhere).
    example_path: &'static str,
    metrics_path: Option<&'static str>,
    /// The product's token scopes, offered after "Read and write".
    extra_scopes: Vec<ScopeChoice>,
    /// Where the new token sheet asks for the password.
    reauth: reauth::View,
}

/// One product scope in the new token sheet.
struct ScopeChoice {
    name: &'static str,
    label: &'static str,
    help: &'static str,
}

fn scope_choices() -> Vec<ScopeChoice> {
    crate::config::get()
        .token_scopes
        .iter()
        .map(|s| ScopeChoice {
            name: s.name,
            label: crate::i18n::t(s.label),
            help: crate::i18n::t(s.help),
        })
        .collect()
}

async fn render(
    core: &Core,
    user: &CurrentUser,
    jar: SignedCookieJar,
    created: Option<(String, String)>,
) -> ApiResult<Response> {
    let uid = user.id;
    let is_admin = user.is_admin();
    let scoped = !core.cfg.token_scopes.is_empty();
    let (mine, all) = core
        .with_conn(move |c| {
            let mut mine = db_tokens::list_for_user(c, uid)?;
            let mut all = if is_admin {
                db_tokens::list_all(c)?
            } else {
                Vec::new()
            };
            if scoped {
                db_tokens::fill_scopes(c, &mut mine)?;
                db_tokens::fill_scopes(c, &mut all)?;
            }
            Ok((mine, all))
        })
        .await?;
    let others = all
        .into_iter()
        .filter(|t| t.user_id != uid)
        .map(TokenRow::from)
        .collect();
    let base_url = core.base_url().await.unwrap_or_default();
    let (ctx, jar) = PageCtx::build(
        core,
        Some(user),
        &jar,
        crate::i18n::t("nav.api_tokens"),
        "settings",
        "tokens",
    )
    .await;
    let (new_token, new_token_name) = match created {
        Some((token, name)) => (Some(token), name),
        None => (None, String::new()),
    };
    let page = TokensTemplate {
        reauth: reauth::View::new(&jar, user, "/settings/tokens"),
        ctx,
        tokens: mine.into_iter().map(TokenRow::from).collect(),
        others,
        new_token,
        new_token_name,
        base_url,
        example_path: crate::docs::spec().example_path,
        metrics_path: crate::docs::spec().metrics_path,
        extra_scopes: scope_choices(),
    };
    // The page may contain a secret: keep it out of caches and history snapshots.
    let mut response = (jar, HtmlTemplate(page)).into_response();
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    Ok(response)
}

pub(super) async fn list(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<Response> {
    user.require(crate::auth::Role::Viewer)?;
    render(&core, &user, jar, None).await
}

#[derive(Deserialize)]
pub struct CreateForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    expires: String,
    /// The creator's current password, needed for [`needs_reauth`] tokens.
    #[serde(default)]
    current_password: String,
}

/// Tokens that outlast a stolen session: write access, or no expiry. The
/// creator confirms their password first; a short-lived read-only token
/// doesn't ask.
fn needs_reauth(scope: TokenScope, expires_in: Option<i64>) -> bool {
    scope == TokenScope::Write || expires_in.is_none()
}

fn back(jar: SignedCookieJar, kind: &str, message: &str) -> Response {
    (flash(jar, kind, message), Redirect::to("/settings/tokens")).into_response()
}

pub(super) async fn create(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Form(form): Form<CreateForm>,
) -> ApiResult<Response> {
    user.require(crate::auth::Role::Viewer)?;
    let name = form.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 80 {
        return Ok(back(jar, "error", crate::i18n::t("tokens.err_name")));
    }
    // "read", "write" or one of the product's scopes (a read-only token
    // that also carries it).
    let chosen = form.scope.trim();
    let (scope, extra) = match TokenScope::parse(chosen) {
        Some(scope) => (scope, None),
        None => match core.cfg.token_scope(chosen) {
            Some(extra) => (TokenScope::Read, Some(extra.name)),
            None => return Ok(back(jar, "error", crate::i18n::t("tokens.err_scope"))),
        },
    };
    let Some(expires_in) = EXPIRY_CHOICES
        .iter()
        .find(|(key, _)| *key == form.expires.trim())
        .map(|(_, days)| *days)
    else {
        return Ok(back(jar, "error", crate::i18n::t("tokens.err_expiry")));
    };
    if needs_reauth(scope, expires_in)
        && let Err(msg) =
            super::security::require_password(&core, &user, &jar, form.current_password).await?
    {
        return Ok(back(jar, "error", &msg));
    }
    let uid = user.id;
    let count = core
        .with_conn(move |c| db_tokens::list_for_user(c, uid))
        .await?
        .len();
    if count >= MAX_TOKENS_PER_USER {
        return Ok(back(jar, "error", crate::i18n::t("tokens.err_limit")));
    }

    let token = tokens::generate();
    let (hash, prefix, stored_name) = (
        tokens::hash(&token),
        tokens::display_prefix(&token),
        name.clone(),
    );
    core.with_conn(move |c| {
        let tx = c.transaction()?;
        let id = db_tokens::insert(&tx, uid, &stored_name, &prefix, &hash, scope, expires_in)?;
        if let Some(extra) = extra {
            db_tokens::add_scope(&tx, id, extra)?;
        }
        tx.commit()
    })
    .await?;
    let scope_name = extra.unwrap_or(scope.as_str());
    tracing::info!(user = %user.username, token = %name, scope = scope_name, "API token created");
    let jar = flash(jar, "success", crate::i18n::t("tokens.created"));
    render(&core, &user, jar, Some((token, name))).await
}

pub(super) async fn revoke(
    State(core): State<Core>,
    user: CurrentUser,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    let token = core
        .with_conn(move |c| db_tokens::get(c, id))
        .await?
        .ok_or(ApiError::NotFound)?;
    // Your own tokens, or anyone's if you're an admin. Don't reveal others' ids.
    if token.user_id != user.id && !user.is_admin() {
        return Err(ApiError::NotFound);
    }
    core.with_conn(move |c| db_tokens::delete(c, id)).await?;
    tracing::info!(user = %user.username, token = %token.name, owner = %token.username, "API token revoked");

    if headers.contains_key("hx-request") {
        let mut response = StatusCode::OK.into_response();
        let value = crate::http::toast_trigger(crate::i18n::t("tokens.revoked"), "success", false);
        response.headers_mut().insert("hx-trigger", value);
        return Ok(response);
    }
    Ok(Redirect::to("/settings/tokens").into_response())
}

#[cfg(test)]
mod tests {
    use askama::Template as _;

    use super::{PageCtx, TokensTemplate};
    use crate::auth::Role;
    use crate::page::UserView;

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
                dir: "ltr",
            },
            tokens: Vec::new(),
            others: Vec::new(),
            new_token: Some("vv_secret".into()),
            new_token_name: "CI".into(),
            base_url: String::new(),
            example_path: "/api/v1/monitors",
            metrics_path: Some("/metrics"),
            extra_scopes: Vec::new(),
            reauth: crate::auth::reauth::View {
                external: false,
                fresh: true,
                url: String::new(),
            },
        };
        let html = page.render().expect("template renders");
        assert_eq!(
            html.matches(r#"id="new-token""#).count(),
            1,
            "duplicate id in tokens page"
        );
        assert!(html.contains(r#"<dialog class="sheet" id="new-token""#));
        assert!(html.contains("vv_secret"));
    }
}
