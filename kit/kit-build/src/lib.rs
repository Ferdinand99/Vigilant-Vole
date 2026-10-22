//! Build-script helper for products built on vole-kit.
//!
//! askama compiles templates into the crate that uses them, so a product
//! whose templates `{% extends "kit/layout.html" %}` needs the kit's
//! template files inside its own package at build time. vole-kit declares
//! `links = "vole-kit"` and exports where its templates are; this helper
//! copies them into the product's `templates/kit/` (which ignores itself in
//! git) and tells Cargo to run again when either side changes. Works the
//! same for a path dependency and a git dependency.
//!
//! In the product's `build.rs` (with `vole-kit` a dependency and
//! `vole-kit-build` a build-dependency):
//!
//! ```text
//! fn main() {
//!     vole_kit_build::templates();
//! }
//! ```

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Where the copy goes, relative to the product's `Cargo.toml`.
pub const DEST: &str = "templates/kit";

/// Copies the kit's templates into `templates/kit/` of the package whose
/// build script calls it. Panics (failing the build with the reason) when
/// the package doesn't depend on vole-kit or a file can't be written.
pub fn templates() {
    let src = std::env::var_os("DEP_VOLE_KIT_TEMPLATES")
        .map(PathBuf::from)
        .expect(
            "DEP_VOLE_KIT_TEMPLATES is not set: vole_kit_build::templates() must run in the build \
         script of a package that depends on vole-kit",
        );
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let dest = Path::new(&manifest).join(DEST);
    sync(&src, &dest).unwrap_or_else(|e| {
        panic!(
            "copying the kit's templates from {} to {}: {e}",
            src.display(),
            dest.display()
        )
    });
    // A changed kit template, or a deleted or edited copy, runs this again.
    println!("cargo::rerun-if-changed={}", src.display());
    println!("cargo::rerun-if-changed={}", dest.display());
    println!("cargo::rerun-if-changed=build.rs");
}

/// Makes `dest` an exact copy of `src` (plus a `.gitignore` that ignores
/// everything). Files whose content is already right are left alone, so
/// their timestamps don't trigger rebuilds.
pub fn sync(src: &Path, dest: &Path) -> io::Result<()> {
    let mut wanted = Vec::new();
    files(src, src, &mut wanted)?;
    fs::create_dir_all(dest)?;
    write_if_changed(
        &dest.join(".gitignore"),
        b"# Copied from vole-kit by vole-kit-build at build time.\n*\n",
    )?;
    for rel in &wanted {
        let data = fs::read(src.join(rel))?;
        let target = dest.join(rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        write_if_changed(&target, &data)?;
    }
    let mut present = Vec::new();
    files(dest, dest, &mut present)?;
    for rel in present {
        if rel != Path::new(".gitignore") && !wanted.contains(&rel) {
            fs::remove_file(dest.join(rel))?;
        }
    }
    Ok(())
}

fn write_if_changed(path: &Path, data: &[u8]) -> io::Result<()> {
    if fs::read(path).ok().as_deref() != Some(data) {
        fs::write(path, data)?;
    }
    Ok(())
}

/// Every file under `dir`, relative to `root`, sorted.
fn files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            files(root, &path, out)?;
        } else {
            out.push(path.strip_prefix(root).expect("under root").to_path_buf());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_copies_updates_and_prunes() {
        let base = std::env::temp_dir().join(format!("vole-kit-build-{}", std::process::id()));
        let (src, dest) = (base.join("src"), base.join("dest"));
        fs::create_dir_all(src.join("partials")).unwrap();
        fs::write(src.join("layout.html"), "L1").unwrap();
        fs::write(src.join("partials/row.html"), "R").unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("stale.html"), "old").unwrap();

        sync(&src, &dest).unwrap();
        assert_eq!(fs::read_to_string(dest.join("layout.html")).unwrap(), "L1");
        assert_eq!(
            fs::read_to_string(dest.join("partials/row.html")).unwrap(),
            "R"
        );
        assert!(!dest.join("stale.html").exists());
        assert!(
            fs::read_to_string(dest.join(".gitignore"))
                .unwrap()
                .contains("*")
        );

        // Unchanged files keep their timestamp; changed ones are rewritten.
        let before = fs::metadata(dest.join("partials/row.html"))
            .unwrap()
            .modified()
            .unwrap();
        fs::write(src.join("layout.html"), "L2").unwrap();
        sync(&src, &dest).unwrap();
        assert_eq!(fs::read_to_string(dest.join("layout.html")).unwrap(), "L2");
        let after = fs::metadata(dest.join("partials/row.html"))
            .unwrap()
            .modified()
            .unwrap();
        fs::remove_dir_all(&base).unwrap();
        assert_eq!(before, after);
    }
}
