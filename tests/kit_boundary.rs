//! The kit never depends on the product: no file under `src/kit/` may name
//! a `crate::` path outside `crate::kit` (so the directory can become its
//! own crate unchanged).

use std::fs;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn violations(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (n, line) in source.lines().enumerate() {
        let code = line.split("//").next().unwrap_or("");
        let mut rest = code;
        while let Some(at) = rest.find("crate::") {
            let before = &rest[..at];
            let after = &rest[at + "crate::".len()..];
            // `$crate::kit::…` in macros is fine too.
            let allowed = after.starts_with("kit::") || after.starts_with('{');
            if !allowed {
                let head: String = after.chars().take(24).collect();
                found.push(format!(
                    "line {}: {}crate::{head}…",
                    n + 1,
                    if before.ends_with('$') { "$" } else { "" }
                ));
            }
            rest = after;
        }
        if code.contains("crate::{") {
            found.push(format!("line {}: crate::{{…}} import", n + 1));
        }
    }
    found
}

#[test]
fn the_kit_does_not_depend_on_the_product() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/kit");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    assert!(!files.is_empty(), "no kit sources found");
    let mut problems = Vec::new();
    for file in files {
        let source = fs::read_to_string(&file).expect("read file");
        for v in violations(&source) {
            problems.push(format!(
                "{}: {v}",
                file.strip_prefix(&root).unwrap().display()
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "src/kit may only use crate::kit paths:\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_checker_catches_what_it_should() {
    assert!(violations("use crate::kit::config::KitConfig;").is_empty());
    assert!(violations("let x = $crate::kit::config::get();").is_empty());
    assert!(violations("// crate::db is fine in a comment").is_empty());
    assert_eq!(violations("use crate::db::models::Role;").len(), 1);
    assert_eq!(violations("crate::i18n::t(\"x\") + crate::api::A").len(), 2);
    assert_eq!(violations("use crate::{kit::a, db::b};").len(), 1);
}
