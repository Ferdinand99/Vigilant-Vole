//! The API reference for people (`/api-docs`), as OpenAPI 3.1 and as
//! Markdown for AI assistants (`/api-docs.md`, `/llms.txt`), all from the
//! product's [`DocsSpec`](super::DocsSpec). Routed by
//! [`crate::auth::pages::router`].

use askama::Template;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum_extra::extract::cookie::SignedCookieJar;

use super::{RouteDoc, groups, instance_url, openapi_document, routes, spec};
use crate::auth::CurrentUser;
use crate::http::rest::{cached_ok, json_ok};
use crate::http::{ApiResult, HtmlTemplate};
use crate::i18n::Tr as _;
use crate::page::PageCtx;
use crate::state::Core;

/// A group of routes, for the template.
pub struct GroupView {
    pub name: &'static str,
    pub anchor: String,
    pub title_id: String,
    pub routes: Vec<RouteDoc>,
}

#[derive(Template)]
#[template(path = "api_docs.html")]
struct DocsTemplate {
    ctx: PageCtx,
    groups: Vec<GroupView>,
    base_url: String,
    error_example: &'static str,
    subtitle: &'static str,
    openapi_path: &'static str,
    api_base: &'static str,
    example_path: &'static str,
    /// For download file names.
    slug: &'static str,
    token_prefix: &'static str,
}

pub(crate) async fn page(
    State(core): State<Core>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let base_url = instance_url(core.base_url().await, &headers);
    let (ctx, jar) = PageCtx::build(
        &core,
        Some(&user),
        &jar,
        crate::i18n::t("docs.title"),
        "",
        "",
    )
    .await;
    let spec = spec();
    let groups = groups()
        .into_iter()
        .map(|g| GroupView {
            name: g,
            anchor: g.to_ascii_lowercase().replace(' ', "-"),
            title_id: format!("{}-title", g.to_ascii_lowercase().replace(' ', "-")),
            routes: routes().iter().filter(|r| r.group == g).copied().collect(),
        })
        .collect();
    Ok((
        jar,
        HtmlTemplate(DocsTemplate {
            ctx,
            groups,
            base_url,
            error_example: spec.error_example,
            subtitle: crate::i18n::t(spec.page_subtitle),
            openapi_path: spec.openapi_path,
            api_base: spec.api_base,
            example_path: spec.example_path,
            slug: core.cfg.slug,
            token_prefix: core.cfg.token_prefix,
        }),
    )
        .into_response())
}

pub(crate) async fn openapi(
    State(core): State<Core>,
    _user: CurrentUser,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let server = core.base_url().await.unwrap_or_else(|| "/".into());
    Ok(json_ok(&headers, &openapi_document(&server)))
}

/// The reference as Markdown, for AI assistants.
pub(crate) async fn markdown_doc(
    State(core): State<Core>,
    _user: CurrentUser,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let base_url = instance_url(core.base_url().await, &headers);
    Ok(cached_ok(
        &headers,
        super::markdown::document(&base_url).into_bytes(),
        "text/markdown; charset=utf-8",
    ))
}

/// The same document at the conventional `llms.txt` address.
pub(crate) async fn llms_txt(
    State(core): State<Core>,
    _user: CurrentUser,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let base_url = instance_url(core.base_url().await, &headers);
    Ok(cached_ok(
        &headers,
        super::markdown::document(&base_url).into_bytes(),
        "text/plain; charset=utf-8",
    ))
}
