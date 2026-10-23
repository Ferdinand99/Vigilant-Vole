//! Serving static files: the kit's design system (stylesheet, scripts,
//! icon sprite, fonts) and the product's own files (its icons and manifest).
//!
//! The design-system files sit in the repository's `static/` folder next to
//! the product's, so both are served from `/static/` exactly as before; the
//! kit embeds its files by name ([`KIT_FILES`]) and the product embeds the
//! rest. A path may belong to only one of them.

use std::borrow::Cow;

use rust_embed::Embed;

use axum::extract::Path as AxumPath;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// One embedded static file.
pub struct StaticFile {
    pub data: Cow<'static, [u8]>,
    pub sha256: [u8; 32],
}

/// The kit's files, as `static/` paths (globs).
pub const KIT_FILES: &[&str] = &[
    "style.css",
    "app.js",
    "theme.js",
    "icons.svg",
    "htmx.min.js",
    "sse.js",
    "passkeys.js",
    "fonts/*",
];

#[derive(Embed)]
#[folder = "../static/"]
#[include = "style.css"]
#[include = "app.js"]
#[include = "theme.js"]
#[include = "icons.svg"]
#[include = "htmx.min.js"]
#[include = "sse.js"]
#[include = "passkeys.js"]
#[include = "fonts/*"]
struct KitAssets;

/// One of the kit's own static files.
pub fn kit_file(path: &str) -> Option<StaticFile> {
    let file = KitAssets::get(path)?;
    Some(StaticFile {
        sha256: file.metadata.sha256_hash(),
        data: file.data,
    })
}

/// Every file the kit serves, sorted.
pub fn kit_paths() -> Vec<String> {
    let mut paths: Vec<String> = KitAssets::iter().map(|p| p.into_owned()).collect();
    paths.sort();
    paths
}

/// `GET /static/{*path}`: the kit's file, else the product's.
pub async fn static_asset(AxumPath(path): AxumPath<String>, headers: HeaderMap) -> Response {
    let file = kit_file(&path).or_else(|| (crate::config::get().static_file)(&path));
    let Some(file) = file else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let etag = format!("\"{}\"", hex::encode(&file.sha256[..16]));
    // Debug builds serve straight from disk, so don't let the browser cache edits.
    let cache_control = if cfg!(debug_assertions) {
        "no-cache"
    } else {
        "public, max-age=3600"
    };
    let not_modified = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag));

    let mut response = if not_modified {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        (
            [(header::CONTENT_TYPE, guess_content_type(&path))],
            file.data,
        )
            .into_response()
    };
    let h = response.headers_mut();
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    response
}

pub fn guess_content_type(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("webmanifest") => "application/manifest+json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("txt") => "text/plain; charset=utf-8",
        Some("map") => "application/json",
        _ => "application/octet-stream",
    }
}
