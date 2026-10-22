//! The kit never depends on the product. Since the kit is its own crate
//! (`kit/`, `vole-kit`), the compiler enforces most of that: kit code can't
//! name `vigilant_vole` unless `kit/Cargo.toml` depends on it. These checks
//! keep that door shut and catch leftovers of the in-crate module.

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

fn kit_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("kit")
}

#[test]
fn the_kit_crate_does_not_depend_on_the_product() {
    let manifest = fs::read_to_string(kit_dir().join("Cargo.toml")).expect("kit/Cargo.toml");
    assert!(manifest.contains("name = \"vole-kit\""), "{manifest}");
    for line in manifest.lines() {
        let key = line.split('=').next().unwrap_or("").trim();
        assert_ne!(
            key, "vigilant-vole",
            "vole-kit must not depend on vigilant-vole"
        );
    }
}

#[test]
fn kit_sources_name_nothing_of_the_product() {
    let mut files = Vec::new();
    rust_files(&kit_dir().join("src"), &mut files);
    assert!(!files.is_empty(), "no kit sources found");
    let mut problems = Vec::new();
    for file in files {
        let source = fs::read_to_string(&file).expect("read file");
        for (n, line) in source.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if code.contains("vigilant_vole") || code.contains("crate::kit") {
                problems.push(format!("{}:{}: {}", file.display(), n + 1, line.trim()));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "kit sources reach into the product:\n{}",
        problems.join("\n")
    );
}

/// The lines of a kit source file that ship: comments and `#[cfg(test)]`
/// modules left out; `testing.rs` and `tests.rs` files are test-only as a
/// whole.
fn shipped_lines(file: &Path, source: &str) -> Vec<(usize, String)> {
    let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name == "testing.rs" || name == "tests.rs" {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut cfg_test = false;
    // Brace depth inside a `#[cfg(test)] mod … { … }` block.
    let mut skipping: Option<i64> = None;
    for (n, line) in source.lines().enumerate() {
        let code = line.split("//").next().unwrap_or("");
        let braces = code.matches('{').count() as i64 - code.matches('}').count() as i64;
        if let Some(depth) = skipping.as_mut() {
            *depth += braces;
            if *depth <= 0 {
                skipping = None;
            }
            continue;
        }
        let trimmed = code.trim();
        if trimmed == "#[cfg(test)]" {
            cfg_test = true;
            continue;
        }
        if cfg_test && !trimmed.is_empty() {
            cfg_test = false;
            if trimmed.starts_with("mod ") || trimmed.starts_with("pub mod ") {
                if braces > 0 {
                    skipping = Some(braces);
                }
                continue;
            }
        }
        out.push((n + 1, code.to_string()));
    }
    out
}

/// `CARGO_PKG_*` in kit code is the kit's own package data, never the
/// product's: product-facing versions and names come from
/// `config::get()` (`version`, `product`, …).
#[test]
fn kit_code_does_not_use_its_own_package_metadata() {
    let mut files = Vec::new();
    rust_files(&kit_dir().join("src"), &mut files);
    let mut problems = Vec::new();
    for file in files {
        let source = fs::read_to_string(&file).expect("read file");
        for (n, code) in shipped_lines(&file, &source) {
            if code.contains("CARGO_PKG_") {
                problems.push(format!("{}:{n}: {}", file.display(), code.trim()));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "kit code uses CARGO_PKG_* (use vole_kit::config::get().version and friends):\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_package_metadata_check_sees_shipped_code_only() {
    let src = "fn v() -> &'static str { env!(\"CARGO_PKG_VERSION\") }\n\
               // env!(\"CARGO_PKG_NAME\") in a comment\n\
               #[cfg(test)]\n\
               mod tests {\n    const V: &str = env!(\"CARGO_PKG_VERSION\");\n}\n\
               const N: &str = env!(\"CARGO_PKG_NAME\");\n";
    let hits: Vec<usize> = shipped_lines(Path::new("x.rs"), src)
        .into_iter()
        .filter(|(_, code)| code.contains("CARGO_PKG_"))
        .map(|(n, _)| n)
        .collect();
    assert_eq!(hits, vec![1, 7]);
    assert!(shipped_lines(Path::new("testing.rs"), src).is_empty());
}
