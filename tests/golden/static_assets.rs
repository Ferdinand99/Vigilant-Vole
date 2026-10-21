//! Every file under `static/` is served at `/static/<path>` with the same
//! content type and content; the paths the templates and manifest reference
//! keep resolving.

use std::path::{Path, PathBuf};

use axum::http::header;
use insta::assert_snapshot;

use crate::harness::App;
use crate::norm;

fn files(dir: &Path, root: &Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path: PathBuf = entry.unwrap().path();
        if path.is_dir() {
            files(&path, root, out);
        } else {
            let rel = path.strip_prefix(root).unwrap().to_string_lossy();
            out.push(rel.replace('\\', "/"));
        }
    }
}

fn is_text(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || content_type.contains("javascript")
        || content_type.contains("json")
        || content_type.contains("svg")
}

#[tokio::test]
async fn served_static_files() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("static");
    let mut paths = Vec::new();
    files(&root, &root, &mut paths);
    paths.sort();

    let mut app = App::fresh("static").await;
    let mut out = String::new();
    for path in &paths {
        let res = app.get(&format!("/static/{path}")).await;
        assert_eq!(res.status, 200, "/static/{path}");
        let content_type = res.header("content-type");
        let disk = std::fs::read(root.join(path)).unwrap();
        assert_eq!(res.body, disk, "/static/{path} serves the file");
        // Text files are hashed with LF line endings (see norm::lf).
        let body = if is_text(&content_type) {
            norm::lf(&res.body)
        } else {
            res.body.clone()
        };
        let cache = res.header("cache-control");
        assert_eq!(
            cache,
            if cfg!(debug_assertions) {
                "no-cache"
            } else {
                "public, max-age=3600"
            },
            "{path}"
        );
        // The ETag is the first 16 bytes of the file's SHA-256, and answers 304.
        let etag = res.header("etag");
        assert_eq!(
            etag,
            format!("\"{}\"", &norm::sha256(&res.body)[..32]),
            "{path}"
        );
        let again = app
            .get_with(
                &format!("/static/{path}"),
                &[("if-none-match", etag.as_str())],
            )
            .await;
        assert_eq!(again.status, 304, "{path}: If-None-Match");
        out.push_str(&format!(
            "/static/{path} | {content_type} | {} bytes | sha256 {}\n",
            body.len(),
            &norm::sha256(&body)[..16]
        ));
    }
    for missing in ["nope.css", "../Cargo.toml", "fonts/", ""] {
        let res = app.get(&format!("/static/{missing}")).await;
        out.push_str(&format!(
            "/static/{missing} -> {} {}\n",
            res.status.as_u16(),
            res.headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
        ));
    }
    assert_snapshot!("static_assets", out);
}

/// What the templates and the web manifest point at must exist.
#[tokio::test]
async fn referenced_assets_resolve() {
    let mut app = App::fresh("static-refs").await;
    let manifest = app.get("/static/site.webmanifest").await;
    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest.body).expect("manifest is JSON");
    let mut refs: Vec<String> = [
        "/static/style.css",
        "/static/app.js",
        "/static/theme.js",
        "/static/htmx.min.js",
        "/static/sse.js",
        "/static/icons.svg",
        "/static/favicon.svg",
        "/static/apple-touch-icon.png",
        "/static/site.webmanifest",
        "/static/fonts/InterVariable.woff2",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for icon in manifest["icons"].as_array().into_iter().flatten() {
        if let Some(src) = icon["src"].as_str() {
            refs.push(src.to_string());
        }
    }
    let mut out = String::new();
    for path in refs {
        let res = app.get(&path).await;
        out.push_str(&format!(
            "{path} -> {} {}\n",
            res.status.as_u16(),
            res.header("content-type")
        ));
    }
    out.push_str(&format!(
        "\nsite.webmanifest:\n{}\n",
        serde_json::to_string_pretty(&manifest).unwrap()
    ));
    assert_snapshot!("static_references", out);
}
