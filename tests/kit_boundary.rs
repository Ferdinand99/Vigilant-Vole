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
