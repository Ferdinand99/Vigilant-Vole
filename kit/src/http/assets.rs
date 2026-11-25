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

/// The files only the kit has, in its own `static/` folder: the side-menu
/// shell (`shell.css` and `shell.js`, used by [`Shell::Sidebar`] pages
/// only). They stay out of `style.css` so that a product on the top bar
/// serves, and changes, nothing. A product may not have files of these names.
///
/// [`Shell::Sidebar`]: crate::page::Shell::Sidebar
#[derive(Embed)]
#[folder = "static/"]
struct KitOwnAssets;

/// One of the kit's own static files.
pub fn kit_file(path: &str) -> Option<StaticFile> {
    let file = KitAssets::get(path).or_else(|| KitOwnAssets::get(path))?;
    Some(StaticFile {
        sha256: file.metadata.sha256_hash(),
        data: file.data,
    })
}

/// Every file the kit serves, sorted.
pub fn kit_paths() -> Vec<String> {
    let mut paths: Vec<String> = KitAssets::iter()
        .chain(KitOwnAssets::iter())
        .map(|p| p.into_owned())
        .collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn text(path: &str) -> String {
        let file = kit_file(path).unwrap_or_else(|| panic!("the kit serves {path}"));
        String::from_utf8(file.data.into_owned()).unwrap()
    }

    #[test]
    fn the_side_menu_shell_is_served_by_the_kit_alone() {
        let paths = kit_paths();
        for path in ["shell.css", "shell.js", "style.css", "app.js"] {
            assert!(paths.iter().any(|p| p == path), "{path}");
        }
        assert_eq!(guess_content_type("shell.css"), "text/css; charset=utf-8");
        assert_eq!(
            guess_content_type("shell.js"),
            "text/javascript; charset=utf-8"
        );
        // The design system's own files are the repository's `static/`; the
        // shell is not among them, so a product on the top bar serves
        // no different stylesheet.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../static");
        assert!(!root.join("shell.css").exists() && !root.join("shell.js").exists());
        assert!(!text("style.css").contains(".side-switch"));
    }

    /// Both themes come from tokens: a colour in the shell is a `var(--…)`
    /// of the stylesheet (each a `light-dark()` pair there), never a literal.
    #[test]
    fn the_shell_stylesheet_takes_its_colours_from_tokens() {
        let shell = text("shell.css");
        let style = text("style.css");
        let mut defined: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for css in [&style, &shell] {
            for line in css.lines() {
                let line = line.trim();
                if let Some(name) = line.strip_prefix("--").and_then(|l| l.split(':').next()) {
                    defined.insert(format!("--{name}"));
                }
            }
        }
        let mut rest = shell.as_str();
        while let Some(at) = rest.find("var(--") {
            let name: String = rest[at + 4..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            assert!(defined.contains(&name), "{name} is no token");
            rest = &rest[at + 6..];
        }
        for (n, line) in shell.lines().enumerate() {
            let code = line.trim();
            if code.starts_with("/*") || code.starts_with('*') || code.starts_with("--brand-text") {
                continue;
            }
            // Hex colours and rgb()/hsl() are for the tokens in style.css.
            let has_hex = code
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '#')
                .any(|w| {
                    w.len() > 1
                        && w.starts_with('#')
                        && w[1..].chars().all(|c| c.is_ascii_hexdigit())
                });
            assert!(
                !has_hex
                    && !code.contains("rgb(")
                    && !code.contains("rgba(")
                    && !code.contains("hsl("),
                "shell.css:{}: a literal colour: {code}",
                n + 1
            );
        }
        // The one token it adds is a pair for both themes.
        assert!(shell.contains("--brand-text: light-dark("));
    }

    /// The page policy has no inline script or style: the shell's script is a
    /// file and uses no way to run text as code.
    #[test]
    fn the_shell_script_runs_no_text_as_code() {
        let js = text("shell.js");
        for banned in [
            "eval(",
            "new Function",
            "innerHTML",
            "document.write",
            "setTimeout(\"",
        ] {
            assert!(!js.contains(banned), "{banned}");
        }
        assert!(!js.contains("style.cssText"));
    }
}
