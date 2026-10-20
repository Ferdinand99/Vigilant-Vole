//! Status page logo and favicon: editor upload/removal and public serving.
//!
//! Uploads are PNG, JPEG, WebP or ICO up to 1 MB, recognised by their magic
//! bytes (the file name and declared type are ignored; SVG is refused). They
//! are stored in the database and served at `/status/{slug}/logo` and
//! `/status/{slug}/favicon` (any host, so custom domains too; a custom domain
//! also answers `/favicon.ico`) with the sniffed `Content-Type`, `nosniff`,
//! a strict CSP and an ETag. Requests carrying the current `?v=` hash are
//! cached for a year; others revalidate.

use askama::Template;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;

use crate::api::status_pages::toast_trigger;
use crate::api::{ApiError, ApiResult, AppState, HtmlTemplate, flash};
use crate::auth::{CurrentUser, Role};
use crate::db::status_pages::extras::{self, Asset, AssetError, AssetKind, MAX_ASSET_BYTES};
use crate::db::status_pages::{self as db_pages, StatusPage};
use crate::i18n::Tr as _;

/// Request body cap for an upload: the file plus multipart overhead.
const UPLOAD_BODY_LIMIT: usize = MAX_ASSET_BYTES + 64 * 1024;

pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/status/{slug}/logo", get(logo))
        .route("/status/{slug}/favicon", get(favicon))
}

/// Editor routes (merged into the protected status page router).
pub fn editor_routes() -> Router<AppState> {
    Router::new().route(
        "/status-pages/{id}/assets/{kind}",
        post(upload).delete(remove).layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
    )
}

// ---------------------------------------------------------------------------
// Serving
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct AssetQuery {
    v: Option<String>,
}

fn etag(asset: &Asset) -> String {
    format!("\"{}\"", &asset.sha256[..asset.sha256.len().min(32)])
}

/// The image response: sniffed type, ETag/304, and a CSP that forbids
/// everything (an image needs none of it, and it blocks content sniffing tricks).
pub fn asset_response(asset: &Asset, kind: AssetKind, versioned: bool, request: &HeaderMap) -> Response {
    let tag = etag(asset);
    let not_modified = request
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == tag || t.trim() == "*"));
    let mut resp = if not_modified {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        (StatusCode::OK, asset.bytes.clone()).into_response()
    };
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(asset.mime));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if versioned { "public, max-age=31536000, immutable" } else { "public, no-cache" }),
    );
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; sandbox"));
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static(match kind {
            AssetKind::Logo => "inline; filename=\"logo\"",
            AssetKind::Favicon => "inline; filename=\"favicon\"",
        }),
    );
    if let Ok(v) = HeaderValue::from_str(&tag) {
        h.insert(header::ETAG, v);
    }
    resp
}

/// Loads an asset of a page visitors may see: published pages for everyone
/// (protected ones included: a logo isn't status data), drafts only for
/// logged-in users.
async fn load_public(state: &AppState, jar: &SignedCookieJar, slug: String, kind: AssetKind) -> ApiResult<Asset> {
    let found = state
        .with_conn(move |c| {
            let Some(page) = db_pages::get_by_slug(c, &slug)? else {
                return Ok(None);
            };
            Ok(Some((page.published, extras::get_asset(c, page.id, kind)?)))
        })
        .await?;
    let (published, asset) = found.ok_or(ApiError::NotFound)?;
    if !published && crate::api::auth::session_user(state, jar).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    asset.ok_or(ApiError::NotFound)
}

async fn serve(state: AppState, jar: SignedCookieJar, headers: HeaderMap, slug: String, kind: AssetKind, v: Option<String>) -> ApiResult<Response> {
    let asset = load_public(&state, &jar, slug, kind).await?;
    let versioned = v.is_some_and(|v| v.len() >= 8 && asset.sha256.starts_with(&v));
    Ok(asset_response(&asset, kind, versioned, &headers))
}

async fn logo(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(q): Query<AssetQuery>,
) -> ApiResult<Response> {
    serve(state, jar, headers, slug, AssetKind::Logo, q.v).await
}

async fn favicon(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path(slug): Path<String>,
    Query(q): Query<AssetQuery>,
) -> ApiResult<Response> {
    serve(state, jar, headers, slug, AssetKind::Favicon, q.v).await
}

/// `/favicon.ico` on a custom domain: the page's favicon, if it has one.
pub async fn custom_domain_favicon(state: &AppState, slug: String, headers: &HeaderMap) -> Option<Response> {
    let asset = state
        .with_conn(move |c| match db_pages::get_by_slug(c, &slug)? {
            Some(p) if p.published => extras::get_asset(c, p.id, AssetKind::Favicon),
            _ => Ok(None),
        })
        .await
        .ok()
        .flatten()?;
    Some(asset_response(&asset, AssetKind::Favicon, false, headers))
}

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

/// The "Logo and icon" section of the settings tab (swapped in place by htmx).
#[derive(Template)]
#[template(path = "partials/status_page_assets.html")]
pub struct AssetsPartial {
    pub page: StatusPage,
    pub rows: Vec<AssetRow>,
    /// Error message for the last upload, shown above the rows.
    pub error: String,
}

/// One row (logo or favicon) of [`AssetsPartial`].
#[derive(Debug, Clone)]
pub struct AssetRow {
    pub kind: &'static str,
    pub icon: &'static str,
    pub title: &'static str,
    pub sub: &'static str,
    /// Preview path when uploaded.
    pub src: Option<String>,
    pub upload_label: &'static str,
    pub remove_label: &'static str,
    pub remove_question: &'static str,
}

impl AssetsPartial {
    pub fn new(page: StatusPage, error: String) -> Self {
        let t = crate::i18n::t;
        let rows = vec![
            AssetRow {
                kind: "logo",
                icon: "globe",
                title: t("spx.logo"),
                sub: if page.logo_sha.is_some() { t("spx.logo_set") } else { t("spx.logo_help") },
                src: page.logo_url(),
                upload_label: t("spx.upload_logo"),
                remove_label: t("spx.remove_logo"),
                remove_question: t("spx.remove_logo_q"),
            },
            AssetRow {
                kind: "favicon",
                icon: "logo",
                title: t("spx.favicon"),
                sub: if page.favicon_sha.is_some() { t("spx.favicon_set") } else { t("spx.favicon_help") },
                src: page.favicon_url(),
                upload_label: t("spx.upload_favicon"),
                remove_label: t("spx.remove_favicon"),
                remove_question: t("spx.remove_favicon_q"),
            },
        ];
        AssetsPartial { page, rows, error }
    }

    /// Rendered HTML, for embedding in the full editor page.
    pub fn html(self) -> ApiResult<String> {
        Ok(self.render()?)
    }
}

/// A multipart read error: over the body limit, or a broken upload.
fn read_error(err: &axum::extract::multipart::MultipartError) -> AssetError {
    if err.status() == StatusCode::PAYLOAD_TOO_LARGE { AssetError::TooLarge } else { AssetError::Empty }
}

/// Reads the `file` field, stopping one byte past the limit.
async fn read_file(mut mp: Multipart) -> Result<Vec<u8>, AssetError> {
    loop {
        let mut field = match mp.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => return Err(AssetError::Empty),
            Err(e) => return Err(read_error(&e)),
        };
        if field.name() != Some("file") {
            continue;
        }
        let mut bytes = Vec::new();
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    bytes.extend_from_slice(&chunk);
                    if bytes.len() > MAX_ASSET_BYTES {
                        return Err(AssetError::TooLarge);
                    }
                }
                Ok(None) => return Ok(bytes),
                Err(e) => return Err(read_error(&e)),
            }
        }
    }
}

fn parse_kind(kind: &str) -> ApiResult<AssetKind> {
    AssetKind::parse(kind).ok_or(ApiError::NotFound)
}

fn saved_message(kind: AssetKind, removed: bool) -> &'static str {
    match (kind, removed) {
        (AssetKind::Logo, false) => crate::i18n::t("spx.logo_saved"),
        (AssetKind::Logo, true) => crate::i18n::t("spx.logo_removed"),
        (AssetKind::Favicon, false) => crate::i18n::t("spx.favicon_saved"),
        (AssetKind::Favicon, true) => crate::i18n::t("spx.favicon_removed"),
    }
}

/// htmx gets the re-rendered section (200 even for validation errors, so it
/// swaps); plain form posts get a flash and a redirect back to settings.
async fn respond(
    state: &AppState,
    jar: SignedCookieJar,
    headers: &HeaderMap,
    id: i64,
    result: Result<&'static str, String>,
) -> ApiResult<Response> {
    if !headers.contains_key("hx-request") {
        let jar = match &result {
            Ok(msg) => flash(jar, "success", *msg),
            Err(err) => flash(jar, "error", err.clone()),
        };
        return Ok((jar, Redirect::to(&format!("/status-pages/{id}/edit?tab=settings"))).into_response());
    }
    let page = state
        .with_conn(move |c| db_pages::get(c, id))
        .await?
        .ok_or(ApiError::NotFound)?;
    let (error, toast) = match result {
        Ok(msg) => (String::new(), Some(msg)),
        Err(err) => (err, None),
    };
    let mut resp = HtmlTemplate(AssetsPartial::new(page, error)).into_response();
    if let Some(msg) = toast {
        resp.headers_mut().insert("hx-trigger", toast_trigger(msg, "success", false));
    }
    Ok(resp)
}

async fn upload(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path((id, kind)): Path<(i64, String)>,
    mp: Multipart,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let kind = parse_kind(&kind)?;
    state.with_conn(move |c| db_pages::get(c, id)).await?.ok_or(ApiError::NotFound)?;
    let result = match read_file(mp).await.and_then(extras::validate_image) {
        Ok(asset) => {
            state.with_conn(move |c| extras::set_asset(c, id, kind, &asset)).await?;
            Ok(saved_message(kind, false))
        }
        Err(e) => Err(e.message().to_string()),
    };
    respond(&state, jar, &headers, id, result).await
}

async fn remove(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    headers: HeaderMap,
    Path((id, kind)): Path<(i64, String)>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let kind = parse_kind(&kind)?;
    state.with_conn(move |c| db_pages::get(c, id)).await?.ok_or(ApiError::NotFound)?;
    state.with_conn(move |c| extras::delete_asset(c, id, kind)).await?;
    respond(&state, jar, &headers, id, Ok(saved_message(kind, true))).await
}
