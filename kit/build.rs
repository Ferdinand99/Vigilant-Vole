//! Tells products where the kit's templates are: with `links = "vole-kit"`,
//! a dependent package's build script sees this as `DEP_VOLE_KIT_TEMPLATES`
//! (`vole-kit-build` copies them into the product's `templates/kit/`).

fn main() {
    let dir =
        std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
            .join("templates");
    println!("cargo::rerun-if-changed=templates");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::metadata=templates={}", dir.display());
}
